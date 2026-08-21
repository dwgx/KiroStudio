//! Tests for `AdminService`. Loaded via `#[path]` from `service.rs`.

use super::*;

#[cfg(test)]
mod multi_open_copies_tests {
    //! 多开份数归一。份数是**外部可控输入**且直接决定本次请求会建多少条凭据，
    //! 故硬上限必须有测试锁住 —— 去掉 clamp 后 `copies_above_cap_is_clamped` 必失败。
    use super::balance_cache_tests::mk_service_with_one_credential;
    use super::super::*;

    #[test]
    fn absent_or_one_means_normal_single_add() {
        // 字段缺失（老客户端 / 普通上号）必须等价于 1，行为与该字段不存在时完全一致。
        assert_eq!(effective_copies(None), 1);
        assert_eq!(effective_copies(Some(1)), 1);
    }

    #[test]
    fn zero_is_normalized_to_one_not_zero_copies() {
        // 0 若原样透传会让 `2..=0` 成为空区间——第 1 份仍建、循环不执行，
        // 结果"看起来对"但语义含糊。显式归一为 1，让 0 与缺失同义。
        assert_eq!(effective_copies(Some(0)), 1);
    }

    #[test]
    fn copies_above_cap_is_clamped() {
        // ⭐ 承重断言：无上限时 `{"copies": 999}` 会真建 999 条同账号凭据，
        // 而它们共用一份上游配额 —— 不是更高并发，是把调度器塞满。
        assert_eq!(effective_copies(Some(999)), MAX_CREDENTIAL_COPIES);
        assert_eq!(effective_copies(Some(u32::MAX)), MAX_CREDENTIAL_COPIES);
        // 边界：正好等于上限时不被改动。
        assert_eq!(
            effective_copies(Some(MAX_CREDENTIAL_COPIES)),
            MAX_CREDENTIAL_COPIES
        );
    }

    #[test]
    fn typical_multi_open_value_passes_through() {
        assert_eq!(effective_copies(Some(4)), 4);
    }

    /// ⭐ 源码级守卫：多开时 **`api_region` 必须继承父号**。
    ///
    /// 这是一条**线上真实发生过**的缺陷，而且我自己先误判成了「这个 key 不支持分身」：
    ///
    /// 分身请求通常只带 `authMethod` + `kiroApiKey` + `copies`，于是 `api_region` 为 None。
    /// 而 CLI 端点的 host 是 `q.{api_region}.amazonaws.com`（`endpoint/cli.rs`），
    /// 拿不到就回退 config 默认（us-east-1）—— 但 `ksk_` token 是**按 region 授权**的，
    /// 于是上游回 403 `AccessDeniedException: The bearer token included in the request is invalid.`
    ///
    /// 实测对照（同一个 key、同一批代理）：
    /// - 不继承 region → 4 个分身 **0% 成功、100% auth_failed**
    /// - 继承 region   → 同一批分身 **83% / 45% / 100% / 88%**
    ///
    /// 用源码守卫而非行为测试：`add_credential` 会打真实上游（`get_usage_limits_for`），
    /// 而本仓铁律禁止测试依赖网络。
    /// `POST /credentials/{id}/api-region` 必须存在且挂在鉴权路由树内。
    ///
    /// 补的是真实运维缺口：`ksk_` 按 region 授权、打错区恒 403 且**永不自愈**，
    /// 而此前全仓没有任何修改 `api_region` 的入口 —— `/regions` 与 `/switch-region`
    /// 都是 ARN 门控（只对有 `profileArn` 的 external_idp 号有意义）⇒
    /// api_key 号 region 错了**只能删号重建**。
    /// 实测 2026-08-05 02:42：4 个分身因缺 region 被打成 `TooManyFailures`，
    /// 运维手上没有「补 region 再启用」的手段。
    #[test]
    fn api_region_setter_endpoint_is_wired() {
        let router = include_str!("router.rs");
        // ⚠️ 判据必须**对空白不敏感**：原写法把路径与 handler 拼成一整行去 contains，
        // 而 rustfmt 会把这条 `.route(..)` 拆成三行（超过 fn_call_width）⇒ 一跑 fmt 就
        // 假红。这不是路由掉了，是守卫写脆了。折叠空白后再比，语义（路径→handler 的
        // 绑定关系）一个不少。同文件的 `clone_endpoint_is_registered_in_router`
        // 是分开断言的，同一意图两种写法，这里对齐成不脆的那种。
        let compact: String = router.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接，避免 include_str! 自匹配。
        let route = format!(
            "\"/credentials/{{id}}/api-region\",post(set_credential_api_region{}",
            ")"
        );
        assert!(
            compact.contains(&route),
            "必须注册 POST /credentials/{{id}}/api-region，否则 api_key 号 region 错了只能删号重建"
        );
        // 校验必须存在：污染值会拼出 q.{垃圾}.amazonaws.com / runtime.{垃圾}.kiro.dev，
        // DNS 失败或 502 —— 而那个失败长得像「号坏了」，会把排查带偏。
        let tm = include_str!("../kiro/token_manager.rs");
        let cut = tm.find("#[cfg(test)]").unwrap_or(tm.len());
        let prod = &tm[..cut];
        let fname = format!("pub fn set_credential_api_region{}", "(");
        let fi = prod
            .find(&fname)
            .expect("token_manager 侧 setter 不该被改名");
        let body_end = prod[fi..]
            .find("\n    pub fn ")
            .map(|i| i + fi)
            .unwrap_or(prod.len());
        let body = &prod[fi..body_end];
        let guard = format!("is_supported_region{}", "(r)");
        assert!(
            body.contains(&guard),
            "setter 必须过 is_supported_region 白名单：污染 region 会拼出无法解析的 host，\
             而那个失败长得像「号坏了」会把排查带偏"
        );
    }

    /// 🔴 承重：`AccountThrottled` **绝不能**导致 `new_cred.disabled = true`。
    ///
    /// # 为什么这条是承重的（改成禁用会造成真实损失）
    ///
    /// `AccountThrottled` 的语义是「**探不了**」（403 账户级临时风控挡在 region 授权校验之前，
    /// 拿不到任何 region 信息），与 `NoUsableRegion`（探过了、确定不行）是两种不同结论。
    ///
    /// 一旦禁用：`ids_needing_region_probe` 过滤 `!e.disabled` ⇒ **连重启时的存量回填都不再
    /// 重探**它，风控过去了也永远不会自愈 ⇒ 临时态被固化成需人工的永久态。
    /// 而不禁用的最坏态只是退回「探测接入前的基线」（`api_region=None` → 回退 `config.region`），
    /// 且若真打错区会走 `report_failure` → `TooManyFailures` ——
    /// **那个原因在 `is_self_healable_reason` 白名单里**，是可自愈的。
    /// 即不禁用的最坏态**严格优于**禁用。
    ///
    /// 严重度：这类 403 占近 2h 流量 22.3%（CLAUDE.md），是常态不是罕见；而
    /// `MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE = 6` 存在的唯一理由就是
    /// 「见过一次 403 不足以判死」—— 探测路径若用一次 403 就判死，等于绕过那道阈值。
    ///
    /// 用源码守卫而非行为测试：`add_credential` 会打真实上游（`get_usage_limits_for`），
    /// 本仓铁律禁止测试依赖网络。
    #[test]
    fn account_throttled_must_not_disable_credential() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];

        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配
        // （本文件已有两处守卫因此踩过坑，见它们的注释）。
        let throttled = format!("region_probe_throttled = {}", "matches!(");
        let ti = prod
            .find(&throttled)
            .expect("AccountThrottled 必须被单独识别，不能与 region_probe_failed 混为一谈");

        // 🔴 核心断言：禁用那句必须**只**在 region_probe_failed 的 if 里，
        // 且必须出现在 throttled 判定**之前** —— 若有人把 throttled 并进那个 matches!，
        // 禁用就会连带作用到它身上。
        let disable = format!("new_cred.disabled = {}", "true;");
        let di = prod.find(&disable).expect("禁用语句不该被改名");
        assert!(
            di < ti,
            "禁用必须发生在 AccountThrottled 判定之前（即只属于 region_probe_failed 那条）——\
             若顺序反了或两者被并进同一个 matches!，被风控的号会被永久禁用且不再重探"
        );

        // AccountThrottled 不得出现在决定禁用的那个 matches! 里。
        let failed_marker = format!("region_probe_failed = {}", "matches!(");
        let fi = prod
            .find(&failed_marker)
            .expect("region_probe_failed 不该被改名");
        let failed_block = &prod[fi..di];
        assert!(
            !failed_block.contains("AccountThrottled"),
            "AccountThrottled 绝不能进 region_probe_failed 的 matches! —— \
             那等于让「探不了」和「确定不行」同样被禁用（见本测试文档的损失论证）"
        );

        // 跳过订阅等级探测的那道门必须**同时**覆盖两者：被风控的号打 management.* 查订阅
        // 同样 403，白付一次上游往返，而上号是用户交互路径。
        let skip_gate = format!("if region_probe_failed || {}", "region_probe_throttled");
        assert!(
            prod.contains(skip_gate.as_str()),
            "跳过订阅等级探测的门必须同时覆盖 AccountThrottled（否则白付一次必然 403 的往返）"
        );
    }

    /// 🔴 region 探测的结果必须**回写进 `new_cred`**，且必须在分身循环**之前**。
    ///
    /// # 实测事故（2026-08-05 02:42）
    ///
    /// 父号 #525 被探测写上 `eu-central-1`（95% 成功），而同批 4 个分身 #526–529
    /// 全部 `api_region=None` ⇒ 回退 `config.region=us-east-1` ⇒ `ksk_` 按区授权
    /// ⇒ 恒 403 `bearer token invalid` ⇒ **24 秒内三次失败全部被禁用、0% 成功**。
    ///
    /// 根因：`for seq in 2..=copies` 里 `new_cred.clone()` 克隆的是**探测前**的
    /// 局部副本。探测只写了 entry，没写这个局部变量。
    ///
    /// ⚠️ 这个缺陷是**接入探测才引入的**：探测之前父子都没 region、一起废（症状一致）；
    /// 接入之后变成「父好子坏」，更容易被误判成「这个 key 不支持分身」。
    ///
    /// 位置断言是承重的：回写若放在分身循环**之后**，等于没回写。
    #[test]
    fn probed_region_must_be_written_back_before_clone_loop() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配。
        let writeback = format!("new_cred.api_region = {}", "Some(probed)");
        let wi = prod.find(&writeback).expect(
            "探测结果必须回写进 new_cred，否则分身克隆的是探测前的副本 ⇒ \
             父号有 region、分身没有 ⇒ ksk_ 打错区恒 403 ⇒ 分身 0% 成功",
        );
        // ⚠️ 必须匹配**代码**而非注释：本文件里有两处注释在散文里提到这个循环
        // （`:1294` 与 `:1413`），裸用 "for seq in 2..=copies" 会先命中注释、
        // 让位置比较反向 → 守卫静默失效（我第一版就是这样，回退验证时才发现）。
        // 故带上循环体的左花括号。
        let loop_marker = format!("for seq in 2..=copies {}", "{");
        let li = prod.find(&loop_marker).expect("分身循环不该被改名");
        assert!(wi < li, "回写必须在分身循环之前（放之后等于没回写）");
        // 且必须在探测调用之后 —— 放之前读到的还是 None。
        let probe = format!("probe_and_persist_api_region{}", "(credential_id)");
        let pi = prod.find(&probe).expect("探测调用不该被改名");
        assert!(pi < wi, "回写必须在探测调用之后，否则读到的仍是探测前的值");
    }

    /// ⚠️ 本条此前**缺 `#[test]`、从未运行过** —— 属性被上一条测试的文档块吃掉了
    /// （2026-08-06 全仓扫出 2 处同型，另一处在 `provider.rs`）。补属性时它一次通过，
    /// 说明它守的东西一直是对的，只是守卫本身没生效。
    #[test]
    fn multi_open_must_inherit_api_region_from_parent() {
        let src = include_str!("service.rs");
        // needle 运行时拼接，避免字面量把自己也算进匹配（同 provider.rs 那个守卫的教训）。
        let needle = format!("{}{}", "api_region: ", "inherit(req.api_region");
        assert!(
            src.contains(needle.as_str()),
            "多开必须继承父号的 api_region：否则分身打到错误的 region host，\
             ksk_ token 按 region 授权 → 上游 403 bearer token invalid → 分身 0% 成功"
        );
        // 同族的另外两个 region 字段一并锁住（三者共同决定路由与认证 region）。
        for f in ["region", "auth_region"] {
            let n = format!("{}: {}", f, "inherit(req.");
            assert!(
                src.contains(n.as_str()),
                "多开也应继承 {f}（与 api_region 同族，共同决定路由/认证 region）"
            );
        }
    }

    /// ⭐ 源码级守卫：`copies` **显式给值时第 1 份也必须绕过去重**。
    ///
    /// 单测覆盖不到 `add_credential`（它会调 `get_usage_limits_for`，那是真实上游网络往返，
    /// 本仓铁律禁止测试依赖网络）。故用源码断言。
    ///
    /// 回归的是一个**实测走不通的场景**：号池里已有 #419/#420，想给它们各加 4 个分身
    /// （不同 machineId + 不同代理出口 IP）。若第 1 份走去重，它撞
    /// `凭据已存在（kiroApiKey 重复）` → 整个请求失败 → 一个分身也建不出来。
    ///
    /// 判据是**归一后份数 > 1**（`is_multi_open`），不是「字段是否出现」——
    /// 见 `copies_equal_one_must_not_bypass_dedup_or_create_a_group`。
    /// 但真多开时第 1 份仍必须绕，这条锁的就是这半边。
    #[test]
    fn explicit_copies_must_bypass_dedup_for_first_copy_too() {
        let src = include_str!("service.rs");
        // needle 运行时拼接：写成完整字面量时它会出现在 include_str! 读到的本测试自身里。
        let judgement = format!("{}{}", "let allow_dup = ", "is_multi_open;");
        let block = src
            .split(judgement.as_str())
            .nth(1)
            .expect("allow_dup 的判据必须是 is_multi_open（归一后份数 > 1）");
        let block = block
            .split("map_err(|e| self.classify_add_error(e))")
            .next()
            .expect("第 1 份的错误处理不应被改动");
        assert!(
            block.contains("add_credential_allowing_duplicate"),
            "真多开（份数 >1）时第 1 份必须走 add_credential_allowing_duplicate，\
             否则给已存在的号加分身会在第 1 份就 bail"
        );
    }

    /// ⭐ 源码级守卫：去重绕过与分身组都必须挂在 `is_multi_open` 上，
    /// 而**不是** `req.copies.is_some()`。
    ///
    /// 回退即 FAIL：把任一处判据改回 `req.copies.is_some()`，下面的否定断言失败。
    ///
    /// 修的是一条静默且不可逆的缺陷：一个总是下发 `"copies": 1` 的 API 客户端
    /// （文档说该字段被 clamp 到 [1,16]，"1 = 普通上号"，所以总是下发 1 是最自然的读法）
    /// 会**永久失去去重保护** —— 重复上号不再报 `凭据已存在`，同一个号在池里越积越多，
    /// 而它们共用一份上游配额；同时每次还造出一个只有 1 个成员的分身组，
    /// 分身管理页上凭空多出一堆「独苗组」。
    ///
    /// clone_group 那半边只能用源码守卫：走行为测试要让 `add_credential` **成功**，
    /// 而它内部会调 `get_usage_limits_for`（真实上游往返），本仓铁律禁止测试依赖网络。
    /// 去重那半边有对应的行为测试（见下一条，它在 bail 处就返回，不碰网络）。
    #[test]
    fn dedup_bypass_and_clone_group_must_hinge_on_effective_copies() {
        let src = include_str!("service.rs");
        // needle 全部运行时拼接：字面量会被 include_str! 读到自己，让断言失真。
        // 且只看**代码行**：注释里必须能写出这个错误判据（本条与上方那段长注释都要提它），
        // 否则这条否定断言会被自己的文档打成恒失败。
        let bug = format!("{}{}", "req.copies.", "is_some()");
        let offending: Vec<&str> = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains(bug.as_str()))
            .collect();
        assert!(
            offending.is_empty(),
            "判据不得是「字段是否出现」：copies=1 会因此绕过去重并造出 1 人分身组。\
             应改用归一后的份数（effective_copies → is_multi_open）。命中行: {offending:?}"
        );
        let group_judgement = format!("{}{}", "let clone_group = if ", "is_multi_open");
        assert!(
            src.contains(group_judgement.as_str()),
            "clone_group 必须只在归一后份数 >1 时赋值"
        );
        let inherit_judgement = format!("{}{}", "let inherited = if ", "is_multi_open");
        assert!(
            src.contains(inherit_judgement.as_str()),
            "字段继承也只在真多开时进行（与 clone_group 同一判据，避免两处再次分叉）"
        );
    }

    /// ⭐ 承重（行为测试）：`copies: 1` **不得绕过去重**。
    ///
    /// 池里已有 `ksk_test`，再用同一个 key + `copies: Some(1)` 上号必须撞
    /// `凭据已存在（kiroApiKey 重复）`。
    ///
    /// 回退即 FAIL：把 `allow_dup` 判据改回 `req.copies.is_some()` —— 去重被绕过，
    /// 这里变成"添加成功"，`expect_err` 失败。
    ///
    /// 不碰网络：去重在 `add_credential_inner` 的第 2 步就 bail，
    /// 早于第 3 步的刷新与之后的 `get_usage_limits_for`。
    #[tokio::test]
    async fn copies_equal_one_must_not_bypass_dedup() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .add_credential(AddCredentialRequest {
                auth_method: "api_key".into(),
                kiro_api_key: Some("ksk_test".into()),
                copies: Some(1),
                ..Default::default()
            })
            .await
            .expect_err("copies=1 是普通上号，重复的 kiroApiKey 必须被去重拦住");
        let msg = err.to_string();
        assert!(msg.contains("已存在"), "应是去重报错，实际 {msg}");
        assert_eq!(
            svc.token_manager.total_count(),
            1,
            "池里不得多出一条同 key 的凭据"
        );
    }

    /// ⭐ 承重：OAuth 号（social/idc/external_idp）多开必须被拒。
    ///
    /// 回退即 FAIL：删掉 `add_credential` 里那段 `multi_open_rejection_reason` 判断 ——
    /// 本测试的 `expect_err` 失败（请求会继续走到入池与真实上游往返）。
    ///
    /// 为什么必须拒：refreshToken 每次刷新都被上游轮换，N 份带同一个 token →
    /// 先刷新的那份把它作废 → 其余份 invalid_grant 被禁用。用户看到的是
    /// 「分身建好了然后一个个变灰」，且原因写着 refresh_token_invalid，
    /// 极易误判成号被封。
    #[tokio::test]
    async fn multi_open_on_oauth_credential_is_rejected() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .add_credential(AddCredentialRequest {
                auth_method: "social".into(),
                refresh_token: Some("rt_social_xyz".into()),
                copies: Some(3),
                ..Default::default()
            })
            .await
            .expect_err("OAuth 号多开必须被拒");
        assert!(
            matches!(err, AdminServiceError::InvalidCredential(_)),
            "应是 InvalidCredential，实际 {err:?}"
        );
        // ⭐ 承重断言是**报错内容**而不是错误种类：删掉这道门后请求会往下走到
        // `validate_refresh_token`，那里同样返回 InvalidCredential（「refreshToken 已被截断」），
        // 只看种类的话缺陷重现了测试照样过。必须断言这条错误确实是"多开不适用"那一条。
        let msg = err.to_string();
        assert!(
            msg.contains("refreshToken 每次刷新都会被上游轮换") && msg.contains("ksk_"),
            "错误必须说清原因（refreshToken 轮换）与适用范围（ksk_），实际: {msg}"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            1,
            "被拒的请求不得留下任何新凭据"
        );
    }

    /// 拒绝判据本身的正反两面（纯函数，不碰网络）。
    #[test]
    fn multi_open_rejection_applies_only_to_non_api_key_credentials() {
        let mut api_key = KiroCredentials::default();
        api_key.auth_method = Some("api_key".into());
        api_key.kiro_api_key = Some("ksk_abc".into());
        assert!(
            multi_open_rejection_reason(&api_key).is_none(),
            "api_key 号没有 refreshToken，多开是安全的，不得被这道检查拦住"
        );

        for method in ["social", "idc", "external_idp"] {
            let mut oauth = KiroCredentials::default();
            oauth.auth_method = Some(method.into());
            oauth.refresh_token = Some("rt".into());
            let reason = multi_open_rejection_reason(&oauth)
                .unwrap_or_else(|| panic!("{method} 号多开必须被拒"));
            assert!(
                reason.contains(method),
                "拒绝理由应点明 authMethod，实际: {reason}"
            );
        }
    }

    // ---------------- M9：region 探测窗口保护 ----------------

    /// 探测窗口判据矩阵（纯函数，不碰网络）：只有「真的会被探测」的号才需要
    /// 临时禁用 —— api_key 号 + region 三字段全空 + 非 custom_api。
    ///
    /// 镜像 `token_manager::needs_api_region_probe` 的逐字判据；行为测试跑不到
    /// 真实探测（上游往返，本仓铁律），故矩阵锁住「哪些号进窗口保护」。
    #[test]
    fn probe_window_guard_judgement_matrix() {
        fn cred(region: Option<&str>, api_region: Option<&str>, auth_region: Option<&str>) -> KiroCredentials {
            KiroCredentials {
                auth_method: Some("api_key".into()),
                kiro_api_key: Some("ksk_m9".into()),
                region: region.map(String::from),
                auth_region: auth_region.map(String::from),
                api_region: api_region.map(String::from),
                ..Default::default()
            }
        }

        // 无任何 region 字段的 api_key 号 → 会探测 → 必须进窗口保护。
        assert!(needs_probe_window_guard(&cred(None, None, None)));
        // 任一 region 字段有值 → probe 直接 Skipped → 不进保护（行为零变化）。
        assert!(!needs_probe_window_guard(&cred(Some("eu-central-1"), None, None)));
        assert!(!needs_probe_window_guard(&cred(None, Some("us-east-1"), None)));
        assert!(!needs_probe_window_guard(&cred(None, None, Some("eu-central-1"))));
        // OAuth 号（无 kiro_api_key）→ probe Skipped → 不进保护。
        let mut oauth = cred(None, None, None);
        oauth.kiro_api_key = None;
        oauth.auth_method = Some("social".into());
        oauth.refresh_token = Some("rt".into());
        assert!(!needs_probe_window_guard(&oauth));
        // custom_api 号（即使旧数据带了 kiro_api_key）→ 不属于 Kiro region 体系 → 不进保护。
        let mut custom = cred(None, None, None);
        custom.auth_method = Some("custom_api".into());
        assert!(!needs_probe_window_guard(&custom));
        // 旧数据兜底：base_url 有值也算 custom_api（is_custom_api_credential 判据）。
        let mut legacy_custom = cred(None, None, None);
        legacy_custom.base_url = Some("https://relay.example.com".into());
        assert!(!needs_probe_window_guard(&legacy_custom));
    }

    /// ⭐ 源码级守卫（M9 承重）：探测窗口内凭据**不可被调度**。
    ///
    /// 线上事故（2026-08-05 05:41）：#536–550 以启用态入池，探测 1-2s 的窗口里
    /// 真实流量打到错区恒 403，3 次即自动禁用 —— 号在自己 region 被探出来之前就死了。
    /// 修复 = 探测前置临时禁用 + 探测后按结论恢复；守卫锁住这个结构：
    ///   1. `probe_and_persist_api_region(credential_id)` 调用**之前**必须存在
    ///      临时禁用赋值（`new_cred.disabled = orig_disabled || will_probe`）。
    ///   2. 探测调用**之后**必须存在恢复调用（`set_disabled(credential_id, false)`）。
    ///
    /// 回退即 FAIL：把临时禁用行删掉 / 把恢复调用删掉 / 把临时禁用挪到探测调用之后
    /// （那等于没保护）。行为测试测不到（真实探测是上游往返），故锁源码。
    #[test]
    fn probe_window_keeps_credential_unselectable() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fname = format!("async fn add_credential_with_intent{}", "(");
        let start = prod.find(&fname).expect("add_credential_with_intent 不应被改名");
        let body_end = prod[start..]
            .find("\n    pub async fn ")
            .map(|i| i + start)
            .unwrap_or(prod.len());
        let body = &prod[start..body_end];

        // 1) 探测调用位置
        let probe = format!("probe_and_persist_api_region{}", "(credential_id)");
        let pi = body
            .find(&probe)
            .expect("region 探测调用不应被删除或改名");

        // 2) 临时禁用必须出现在探测**之前**（needle 拼接防自匹配）。
        let guard_assign = format!(
            "new_cred.disabled = {} || will_probe;",
            "orig_disabled"
        );
        let gi = body.find(&guard_assign).unwrap_or_else(|| {
            panic!(
                "入池前必须存在临时禁用赋值（{guard_assign}）——否则探测窗口内\
                 号以启用态在池中，真实流量打错区 3 次即被自动禁用（事故 #536-550）"
            )
        });
        assert!(gi < pi, "临时禁用必须在探测调用之前（放之后等于没保护）");

        // 3) 恢复调用必须出现在探测**之后**。
        let restore = format!("set_disabled(credential_id, {})", "false");
        let ri = body.find(&restore).unwrap_or_else(|| {
            panic!(
                "探测后必须存在恢复启用调用（set_disabled(credential_id, false)）——\
                 否则临时禁用的号永远留在禁用态"
            )
        });
        assert!(pi < ri, "恢复必须在探测完成之后");
    }
}
    /// 🔴 档位切换必须真的落到消费侧（2026-08-11 新增）。
    ///
    /// 完整链路：面板切档 → `throttle_profile` 分支设 `hot_changed=true`
    /// → `hot_or_display_changed` → `reload_config()` → `token_manager` 把
    /// `inbound_queue_timeout_passthrough` 等值 `store` 进 `GlobalThrottle`。
    ///
    /// 断掉任一环的表现都是「面板显示已切档、config.json 里也写对了，但行为没变」——
    /// 而档位管的恰好是「整形层超时放行还是返 429」这种**只在真实压力下才看得出**的开关，
    /// 排障时极难定位。所以这里钉住两点：切档分支存在，且它设了 `hot_changed`。
    #[test]
    fn throttle_profile_switch_is_wired_to_hot_reload() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        // 显式截断测试段：否则本测试自身的字面量会让 split 命中测试代码
        // （本文件已因这类原因出过一次「守卫静默变绿」）。
        let needle_fn = format!("pub fn update{}", "_config");
        let update_fn = src
            .split(needle_fn.as_str())
            .nth(1)
            .and_then(|s| s.split_once("\n#[cfg(test)]").map(|(head, _)| head))
            .expect("找不到 update_config 的生产代码段");

        // ① 切档分支必须在 update_config 里
        let field = format!("req.throttle{}", "_profile");
        assert!(
            update_fn.contains(field.as_str()),
            "update_config 里找不到 {field} 的处理分支 —— 面板切档不会有任何效果"
        );

        // ② 该分支必须设 hot_changed（否则当次不触发 reload_config，改动要等重启才生效）
        let seg = update_fn
            .split(field.as_str())
            .nth(1)
            .expect("上面已断言存在");
        // ⚠️ 窗口必须截到**本分支结束**（下一个 `if let Some` 处），不能用固定字符数。
        // 初版取 600 字符 ⇒ 越过本分支、命中了下一个字段的 `hot_changed = true`
        // ⇒ 删掉切档分支自己那一行，守卫**仍然绿**（实测确认过）。
        // 这正是本守卫要防的失败模式，却先发生在守卫自己身上。
        let next_branch = format!("if let Some{}", "(");
        let window = seg
            .find(next_branch.as_str())
            .map(|end| &seg[..end])
            .unwrap_or(seg);
        let hot = format!("hot{}", "_changed = true");
        assert!(
            window.contains(hot.as_str()),
            "切档分支没有设 {hot} —— 后果：config.json 写对了、面板显示成功，\
             但当次进程内的整形层/冷却开关**不会更新**，要重启才生效。\
             这是本文件历史上出现过的同款隐蔽故障。"
        );
    }


#[cfg(test)]
mod absorb_hot_reload_tests {
    // ⚠️ 2026-08-15：error_messages 校验矩阵测试也挂在本模块（A1 实现）；
    // 子模块不自动继承父级项，必须显式引入（validate_error_messages /
    // ERROR_TABLE_MAX_ENTRIES / HashMap）。
    use super::super::*;
    use std::collections::HashMap;

    /// ⭐ 源码守卫：`absorb_changed` 必须出现在 `hot_or_display_changed` 的 OR 链里。
    ///
    /// 回退即 FAIL：删掉 `update_config` 里那行 `|| absorb_changed`，本测试失败。
    ///
    /// 为什么这条是本方案唯一新增的风险点：吸收层**没有** TIER3 setter（它在 provider 内
    /// 直接读 token_manager 的 config ArcSwap），所以「面板改动生效」这件事完全依赖
    /// `hot_or_display_changed` 触发 `reload_config` 把新配置从盘重读并原子换入 ArcSwap。
    /// 漏掉这一行的表现极其隐蔽：面板显示保存成功、config.json 里确实写进去了、
    /// 重启后也确实生效 —— 唯独**当次不生效**，排障时几乎不可能想到是这里。
    ///
    /// 单测无法真跑 `update_config`（需要真实 TokenManager + 磁盘 config），故用源码断言。
    #[test]
    fn absorb_changed_is_in_hot_reload_or_chain() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        // ⚠️ 显式截断测试段（2026-08-11 审计修复）：`split(...).nth(1)` 只取第二个片段，
        // 此前 update_fn 恰好在本测试自身的 `.split("pub fn update_config")` 字面量处
        // 截断 —— 绿是**巧合**（依赖测试段里存在该字面量），删掉那个字面量 update_fn
        // 会延伸到文件末尾、把本测试断言行的 `absorb_changed = true` 字面量数进去 →
        // 计数 11 ≠ 10 误红。显式截断 + needle 运行时拼接后语义与位置无关。
        let needle_fn = format!("pub fn update{}", "_config");
        let update_fn = src
            .split(needle_fn.as_str())
            .nth(1)
            .and_then(|s| s.split_once("\n#[cfg(test)]").map(|(head, _)| head))
            .expect("update_config 不应被改名");
        // 截到 reload_config 调用处为止，只看它之前的那条 OR 链。
        let or_chain = update_fn
            .split("self.token_manager.reload_config()")
            .next()
            .expect("reload_config 调用点不应被改名");
        let needle = format!("{}{}", "|| absorb_", "changed");
        assert!(
            or_chain.contains(needle.as_str()),
            "hot_or_display_changed 的 OR 链必须包含 absorb_changed，否则面板改了吸收层配置\
             会存盘但不触发 reload_config → ArcSwap 仍是旧值 → 开关当次静默无效"
        );
        // 七个字段都必须真的会把 absorb_changed 置位（防加了字段忘了置位）。
        // 2026-08-10 从六项扩到七项：补入 `upstream_retry_absorb_server_error`
        // —— 它在 `model/config.rs` 早已存在，但此前**没暴露到 Admin API**，
        // 只能改 config.json + 重启。线上代挂上游主要故障形态是 502，
        // 不吸收 5xx 等于把最典型的瞬态故障直接甩给客户端断会话。
        // 2026-08-11 扩到十项：capacity_400 / swap_budget_secs / exhausted_status
        // （同类问题：只存在于 config.json，面板与 API 都改不了）。
        let absorb_fields = [
            "upstream_retry_absorb_enabled",
            "upstream_retry_absorb_budget_secs",
            "upstream_retry_absorb_max_rounds",
            "upstream_retry_absorb_min_delay_ms",
            "upstream_retry_absorb_max_delay_secs",
            "upstream_retry_absorb_suspended",
            "upstream_retry_absorb_server_error",
            "upstream_retry_absorb_capacity_400",
            "upstream_retry_absorb_swap_budget_secs",
            "upstream_retry_absorb_exhausted_status",
        ];
        for field in absorb_fields {
            assert!(
                update_fn.contains(&format!("req.{field}")),
                "update_config 必须读取 req.{field}，否则该字段面板改不了"
            );
        }
        assert_eq!(
            update_fn.matches("absorb_changed = true").count(),
            absorb_fields.len(),
            "每个吸收层字段各自都必须置位 absorb_changed（漏一个 → 只改那个字段时不热更）。\
             新增字段时这里的计数会自动跟着 absorb_fields 走，不用再手改数字"
        );
    }

    /// ⭐ 源码守卫：配置快照的吸收层十项必须**逐字段从 config 读**，不得写死。
    ///
    /// 回退即 FAIL：把任一项改成字面量（如 `upstream_retry_absorb_enabled: false,`），断言失败。
    ///
    /// 为什么这条替代了规格里那条「第三处默认值镜像」的守卫：`ConfigSnapshotResponse`
    /// 其实**没有** `Default` impl（规格与我的设计文档都记错了，把 types.rs 里一个**测试夹具**
    /// 的结构体字面量当成了 Default）。真实的漂移面不是"默认值三处不一致"，而是
    /// "快照有没有真的把 config 的值读出来" —— 写死的话面板永远显示默认值、
    /// 用户改了也看不到变化，而任何只比对默认值的测试都发现不了（默认态下两者恰好相等）。
    #[test]
    fn absorb_snapshot_maps_every_field_from_config() {
        let src = include_str!("service.rs");
        for field in [
            "upstream_retry_absorb_enabled",
            "upstream_retry_absorb_budget_secs",
            "upstream_retry_absorb_max_rounds",
            "upstream_retry_absorb_min_delay_ms",
            "upstream_retry_absorb_max_delay_secs",
            "upstream_retry_absorb_suspended",
            // 2026-08-10 补：该字段此前完全没进 Admin API（面板看不到也改不了）
            "upstream_retry_absorb_server_error",
            // 2026-08-11 补：同类问题三个字段（只存在于 config.json）
            "upstream_retry_absorb_capacity_400",
            "upstream_retry_absorb_swap_budget_secs",
            "upstream_retry_absorb_exhausted_status",
        ] {
            let mapping = format!("{field}: config.{field},");
            assert!(
                src.contains(mapping.as_str()),
                "配置快照必须写 `{mapping}`（逐字段从 config 读）；\
                 写死字面量会让面板永远显示默认值、用户改了也看不到"
            );
        }
    }

    /// 🔴 回归：`auto_disable_suspicious` 必须**三处都接线**（快照 / 更新分支 / 不进重启集）。
    ///
    /// 这个字段此前只存在于 `Config` 与 `TokenManager`：`reload_config` 确实在读它，
    /// 但 `admin/types.rs` 既没有响应字段也没有请求字段，`service.rs` 也没有更新分支
    /// ⇒ **面板既看不到也改不了它**，只能手改 config.json + 重启。
    ///
    /// 实际造成的排查错误：线上有人「把三个自动禁用开关关掉」，而这一项其实改不到，
    /// 于是配置 API 读回 `None`，看起来像"没有这个开关"。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn auto_disable_suspicious_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let types = include_str!("types.rs");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("auto_disable{}", "_suspicious");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的 TIER1 更新分支，否则面板改不动它"
        );
        // 响应结构与请求结构各一处。
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // TIER1 语义守卫：它是热更字段，绝不能进 restart_fields。
        let restart = format!("restart_fields.push(\"{}\"", "autoDisableSuspicious");
        assert!(
            !src.contains(&restart),
            "该字段是 TIER1 热更（reload_config 已读它），不得要求重启"
        );
    }

    /// 🔴 回归（2026-08-15 补接线）：`ota_auto_check` 必须**全套接线**。
    ///
    /// 此前该字段只存在于 `Config` 与 main.rs 启动门控：前端 settings-page.tsx 提交
    /// `otaAutoCheck`，但 ConfigSnapshotResponse / UpdateConfigRequest 都没有它 →
    /// serde 静默丢弃 → 用户开了「自动检查」保存成功却不生效，且快照不下发 →
    /// 刷新后开关恒回弹为关。与已修的 prompt_cache_enabled 事故完全同型。
    ///
    /// 语义是 **restart-only**：main.rs 启动期按 config 门控 spawn 后台检查任务
    /// （无 TIER2 respawn 机制），改后必须重启进程才生效 → 必须进 restart_fields
    /// （前端据此 toast「需重启」），且绝不能进 hot_or_display_changed（restart-only
    /// 纪律，见 build_config_snapshot 的 proxy split-brain 注释）。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn ota_auto_check_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let types = include_str!("types.rs");
        let field = format!("ota_auto{}", "_check");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的更新分支，否则面板改不动它"
        );
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // restart-only 语义守卫：必须进 restart_fields（前端提示重启），
        // 且不得进 hot_or_display_changed 的 reload 触发链。
        let restart = format!("restart_fields.push(\"{}\"", "otaAutoCheck");
        assert!(
            src.contains(&restart),
            "OTA 自动检查是启动期 spawn 的后台任务，必须进 restart_fields 提示重启"
        );
        let hot_chain = format!("{field}_changed");
        assert!(
            !src.contains(&hot_chain),
            "restart-only 字段不得进 hot_or_display_changed 的 reload 触发链（proxy split-brain 纪律）"
        );
    }

    /// 🔴 回归（2026-08-16 新增）：`scheduling_mode` 必须**全套接线**。
    ///
    /// 三按钮方案（docs/scheduling-config-simplify.md §3.2）的前端入口。该字段此前
    /// 不存在，若只加 `Config` 字段而漏掉任一处接线，面板要么读不到（快照缺失）、
    /// 要么改不动（请求结构缺失/无更新分支）—— 与 `ota_auto_check` 事故同型。
    ///
    /// 语义是 TIER1 热更：切换调度模式即写矩阵 + 落盘（save），无需重启。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn scheduling_mode_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let types = include_str!("types.rs");
        let field = format!("scheduling{}", "_mode");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(m) = {update}` 的更新分支，否则面板改不动它"
        );
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // TIER1 语义守卫：它是热更字段，绝不能进 restart_fields。
        let restart = format!("restart_fields.push(\"{}\"", "schedulingMode");
        assert!(
            !src.contains(&restart),
            "该字段是 TIER1 热更（切换即写矩阵 + save 落盘），不得要求重启"
        );
    }

    /// 🔴 回归（2026-08-14 新增）：`auto_disable_quota_exceeded` 必须**全套接线**。
    ///
    /// 该开关是 AdminService **内存态**（不进 config.json），漂移面有三处：
    /// ① 快照（面板读得到当前值）；② `req.{field}` 更新分支（面板改得动）；
    /// ③ 余额刷新循环的读取点（不接线 = 开关形同虚设）。types.rs 响应/请求结构各一处。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn auto_disable_quota_exceeded_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let types = include_str!("types.rs");
        // 折叠空白再比：长链调用会被 rustfmt 拆成多行（同 router 守卫写法）。
        let compact: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        let field = format!("auto_disable{}", "_quota_exceeded");

        let snapshot = format!("{field}:self.{field}");
        assert!(
            compact.contains(&snapshot),
            "配置快照必须输出 `{snapshot}`，否则面板读不到该开关当前值"
        );
        let update = format!("req.{field}");
        assert!(
            compact.contains(&update),
            "必须有 `if let Some(v) = {update}` 的更新分支，否则面板改不动它"
        );
        assert!(
            compact.contains(&format!("{field}.load")),
            "余额刷新循环必须有 `{field}.load(..)` 读取点，否则开关改了也不生效"
        );
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
    }

    /// 🔴 回归：`native_thinking_effort_enabled` 必须**全套接线**（快照 / 更新分支 /
    /// TIER3 setter 应用 / 两条 OR 链），否则面板改了不生效且回「无改动」。
    ///
    /// 参考仓移植的新开关，必须一次性接通才会被面板看到、改到、热更到：
    /// - 快照：`build_config_snapshot` 逐字段从 config 读（否则面板永远显示默认值）；
    /// - 更新分支：`req.{field}` 置位（否则面板改不动）；
    /// - TIER3：改后调 `set_native_thinking_effort_enabled` 写 converter 进程镜像
    ///   （否则存了盘但热路径仍读旧值，开关静默无效）；
    /// - 两条 OR 链各一处（hot_or_display_changed 与 immediate_changed，漏一条 →
    ///   只改本项时面板回「无改动」）。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn native_thinking_effort_enabled_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let types = include_str!("types.rs");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("native_thinking{}", "_effort_enabled");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的 TIER3 更新分支，否则面板改不动它"
        );
        let setter = format!("set_native{}", "_thinking_effort_enabled(v)");
        assert!(
            src.contains(&setter),
            "改后必须调 converter 的 `{setter}` 写进程镜像，否则热路径读旧值"
        );
        // 响应结构与请求结构各一处（快照 + 请求）。
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag。
        assert!(
            src.matches(&format!("|| {field}_changed.is_some()")).count() >= 2,
            "本 flag 必须同时进 hot_or_display_changed 与 immediate_changed 两条 OR 链"
        );
    }

    /// 🔴 回归：`tool_compat_mapping` 必须**全套接线**（快照 / 更新分支 / TIER3 setter
    /// 应用 / 两条 OR 链），否则面板改了不生效且回「无改动」。
    ///
    /// CC↔Kiro 工具名/参数映射开关，此前只有 converter 原子默认 true 无配置入口，
    /// 必须一次性接通才会被面板看到、改到、热更到：
    /// - 快照：`build_config_snapshot` 逐字段从 config 读（否则面板永远显示默认值）；
    /// - 更新分支：`req.{field}` 置位（否则面板改不动）；
    /// - TIER3：改后调 `set_tool_compat_mapping` 写 converter 进程镜像
    ///   （否则存了盘但热路径仍读旧值，开关静默无效）；
    /// - 两条 OR 链各一处（hot_or_display_changed 与 immediate_changed，漏一条 →
    ///   只改本项时面板回「无改动」）。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn tool_compat_mapping_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let types = include_str!("types.rs");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("tool_compat{}", "_mapping");

        let snapshot = format!("{field}: config.{field},");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 `{snapshot}`，否则面板读不到真实值"
        );
        let update = format!("req.{field}");
        assert!(
            src.contains(&update),
            "必须有 `if let Some(v) = {update}` 的 TIER3 更新分支，否则面板改不动它"
        );
        let setter = format!("set_tool{}", "_compat_mapping(v)");
        assert!(
            src.contains(&setter),
            "改后必须调 converter 的 `{setter}` 写进程镜像，否则热路径读旧值"
        );
        // 响应结构与请求结构各一处（快照 + 请求）。
        assert!(
            types.matches(&field).count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches(&field).count()
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag。
        assert!(
            src.matches(&format!("|| {field}_changed.is_some()")).count() >= 2,
            "本 flag 必须同时进 hot_or_display_changed 与 immediate_changed 两条 OR 链"
        );
    }

    /// 🔴 回归：透传模拟缓存必须**全套接线**（快照 / 更新分支 / TIER3 setter 应用 /
    /// 两条 OR 链），否则面板改了不生效且回「无改动」。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。
    #[test]
    fn mock_cache_config_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        // types.rs 的测试段（#[cfg(test)] 之后）含同名字段的访问/构造，会垫底 count ——
        // 先截断测试段再数，count 只反映生产结构（响应 + 请求各一处）。
        let types = include_str!("types.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or("");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let field = format!("mock_cache{}", "_changed");

        // 快照：面板读到的配置快照必须逐字段来自 config（needle 拼接防自证）。
        let snapshot_enabled = format!("mock_cache_enabled: config.mock_cache_{}", "enabled,");
        let snapshot_ratio = format!("mock_cache_read_ratio: config.mock_cache_{}", "read_ratio,");
        assert!(
            src.contains(&snapshot_enabled) && src.contains(&snapshot_ratio),
            "配置快照必须逐字段从 config 读 mock 两字段，否则面板读不到真实值"
        );
        let update_enabled = format!("req.mock_cache_{}", "enabled");
        let update_ratio = format!("req.mock_cache_{}", "read_ratio");
        assert!(
            src.contains(&update_enabled) && src.contains(&update_ratio),
            "必须有更新分支读取 req 两字段，否则面板改不动它"
        );
        // setter 调用存在（needle 拼接，防测试段字面量自证；折叠空白防 rustfmt 拆行）。
        let compact: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        let setter = format!(
            "set_mock_cache{}",
            "_config(config.mock_cache_enabled,config.mock_cache_read_ratio,)"
        );
        assert!(
            compact.contains(&setter),
            "改后必须调 handlers 的 set_mock_cache_config 写进程镜像，否则热路径读旧值"
        );
        // 响应结构与请求结构各一处（快照 + 请求）；测试段已截断，count 只数生产字段。
        assert!(
            types.matches("mock_cache_enabled").count() >= 2
                && types.matches("mock_cache_read_ratio").count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段"
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag。
        // 与 native_thinking 的 `_changed.is_some()` 不同，本 flag 是 bool：needle 为 `|| {field}`。
        assert!(
            src.matches(&format!("|| {field}")).count() >= 2,
            "本 flag 必须同时进 hot_or_display_changed 与 immediate_changed 两条 OR 链"
        );
    }

    /// 🔴 回归：错误码/提示词覆盖表必须**全套接线**（快照 / 更新分支 / 先校验再写盘 /
    /// OR 链 / import_config 同校验），否则面板改了不生效且回「无改动」。
    ///
    /// 回退即 FAIL：删掉任一处接线 → 对应断言失败。needle 全部运行时拼接
    /// （include_str! 会读到本测试自身，防自证绿，守卫纪律见 CURRENT.md）。
    #[test]
    fn error_messages_config_is_fully_wired() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let types = include_str!("types.rs");

        let snapshot = format!("error_messages: config.error_messages{}", ".clone(),");
        assert!(
            src.contains(&snapshot),
            "配置快照必须逐字段从 config 读 error_messages，否则面板读不到真实值"
        );
        let update = format!("req.error{}", "_messages");
        assert!(
            src.contains(&update),
            "必须有更新分支读取 req.error_messages，否则面板改不动它"
        );
        // 函数定义 + 更新分支 + import_config 三处（needle 拼接，防测试段自证）。
        let define = format!("fn validate_error{}", "_messages(");
        assert!(
            src.contains(&define),
            "validate_error_messages 函数必须存在"
        );
        let update_call = format!("validate_error_messages(&merged{}", ")");
        assert!(
            src.contains(&update_call),
            "更新分支必须先调 validate_error_messages（merged）再写盘"
        );
        let import_call = format!("validate_error_messages(&imported.error{}", "_messages)");
        assert!(
            src.contains(&import_call),
            "import_config 必须校验导入的 error_messages（失败整份拒绝零写盘）"
        );
        // 整表拒绝语义：校验失败必须 Err 短路（保持旧表）。
        // ⚠️ 2026-08-15 per-key merge 改造后变量名 em → merged（merge 在赋值前完成），
        // needle 同步更新；语义不变（校验失败 Err 短路 = 旧表不被替换）。
        let err_short = format!(
            "validate_error_messages(&merged).map_err(AdminServiceError::{}",
            "InvalidCredential)?"
        );
        assert!(
            src.contains(&err_short),
            "校验失败必须整表拒绝（Err 短路，保持旧表）"
        );
        // 两条 OR 链（hot_or_display_changed 与 immediate_changed）各必须含本 flag
        // （bool flag：needle `|| {field}`，count>=2 防只进一条链）。
        let or_needle = format!("|| error_messages{}", "_changed");
        assert!(
            src.matches(&or_needle).count() >= 2,
            "error_messages_changed 必须同时进 hot_or_display_changed 与 immediate_changed \
             两条 OR 链：漏 hot 链 → 存盘但热路径读旧表（无 TIER3 setter，这是唯一生效通道）；\
             漏 immediate 链 → 面板只改本项时回「未检测到变更」，与实际不符"
        );
        // 响应结构与请求结构各一处（快照 + 请求）。
        assert!(
            types.matches("error_messages").count() >= 2,
            "types.rs 里响应结构与请求结构都必须有该字段（当前 {} 处）",
            types.matches("error_messages").count()
        );
    }

    // ---- validate_error_messages 校验矩阵（纯函数，不碰网络/磁盘）----

    fn error_entry(
        status: Option<u16>,
        ty: Option<&str>,
        message: Option<&str>,
        ra: Option<u64>,
    ) -> crate::model::error_messages::ErrorMessageOverride {
        crate::model::error_messages::ErrorMessageOverride {
            status,
            r#type: ty.map(str::to_string),
            message: message.map(str::to_string),
            retry_after_secs: ra,
        }
    }

    fn one_error_entry(
        entry: crate::model::error_messages::ErrorMessageOverride,
    ) -> HashMap<String, crate::model::error_messages::ErrorMessageOverride> {
        let mut m = HashMap::new();
        m.insert("test_key".to_string(), entry);
        m
    }

    #[test]
    fn validate_accepts_full_valid_entry() {
        let table = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            Some("请按 Retry-After 退避后重试。"),
            Some(8),
        ));
        assert!(validate_error_messages(&table).is_ok(), "合法条目必须通过");
    }

    #[test]
    fn validate_rejects_status_out_of_whitelist() {
        for bad in [200u16, 418, 451, 529, 600] {
            let table = one_error_entry(error_entry(Some(bad), Some("api_error"), None, None));
            let err = validate_error_messages(&table).expect_err("白名单外的 status 必须整表拒绝");
            assert!(
                err.contains(".status"),
                "错误必须点名 status 字段，实际: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_type_out_of_whitelist() {
        for bad in [
            "service_unavailable",
            "internal_error",
            "upstream_error",
            "bogus_type",
        ] {
            let table = one_error_entry(error_entry(Some(502), Some(bad), None, None));
            let err = validate_error_messages(&table).expect_err("白名单外的 type 必须整表拒绝");
            assert!(err.contains(".type"), "错误必须点名 type 字段，实际: {err}");
        }
    }

    #[test]
    fn validate_rejects_status_type_combination_violation() {
        // 429 → 只允许 rate_limit_error / overloaded_error（billing_error 已移除，
        // 其拒绝在 type 白名单层，见 validate_rejects_billing_error_and_quota_exceeded_error）。
        for bad in [
            ("429", "invalid_request_error"),
            ("429", "api_error"),
            ("429", "not_found_error"),
            ("401", "rate_limit_error"),
            ("403", "authentication_error"),
            ("404", "permission_error"),
            ("400", "overloaded_error"),
            ("413", "rate_limit_error"),
            ("500", "overloaded_error"),
            ("502", "rate_limit_error"),
            ("503", "not_found_error"),
        ] {
            let table = one_error_entry(error_entry(
                Some(bad.0.parse().unwrap()),
                Some(bad.1),
                None,
                None,
            ));
            let err = validate_error_messages(&table).expect_err("组合违例必须整表拒绝");
            assert!(
                err.contains("组合不合法"),
                "错误必须说明组合约束，实际: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_decision_words() {
        // 决策词黑名单：任一命中 → 拒（设计 §二 5）。
        for bad in [
            "credit balance is too low",
            "organization has been disabled",
            "message says overloaded_error here",
            "Monthly quota exhausted",
            "this account has billing issues",
        ] {
            let table = one_error_entry(error_entry(
                Some(429),
                Some("rate_limit_error"),
                Some(bad),
                None,
            ));
            assert!(
                validate_error_messages(&table).is_err(),
                "决策词必须拒绝: {bad}"
            );
        }
        // quota+exhausted 无豁免（B2）：billing_error 已从白名单移除，旧豁免条件
        // 永远不可达——配什么 type 都拒（Claude Code CLI 层 D 判定/opencode 模式
        // 匹配都拿 quota+exhausted 当重试决策输入）。
        let rejected = one_error_entry(error_entry(
            Some(429),
            Some("billing_error"),
            Some("Monthly quota exhausted"),
            None,
        ));
        assert!(
            validate_error_messages(&rejected).is_err(),
            "quota+exhausted 必须无条件拒绝（billing_error 已不可配置，无豁免）"
        );
    }

    #[test]
    fn validate_rejects_retry_after_out_of_range() {
        let table = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            None,
            Some(3601),
        ));
        let err = validate_error_messages(&table).expect_err("retryAfterSecs 超 3600 必须拒绝");
        assert!(
            err.contains("retryAfterSecs"),
            "错误必须点名 retryAfterSecs，实际: {err}"
        );
        let ok = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            None,
            Some(3600),
        ));
        assert!(validate_error_messages(&ok).is_ok(), "3600 是边界合法值");
    }

    #[test]
    fn validate_accepts_load_bearing_message_with_warning() {
        // 承重字符串：提示不硬拒（shield COOLING_MARKERS 三哨兵 / prompt is too long / 背压哨兵）。
        // ⚠️ 2026-08-15 勘误：「等容量」不是 shield 判据（仅注释出现），已从词表移除——
        // 含它的普通文案照常放行（见 error_messages.rs 词表测试）。
        for keep in [
            "All credentials are temporarily cooling down. Please retry.",
            "Gateway inbound rate shaping is at capacity; retrying immediately will not help.",
            "prompt is too long: 上下文窗口已满",
            "This is gateway-side backpressure; retrying immediately will not help.",
        ] {
            let table = one_error_entry(error_entry(
                Some(400),
                Some("invalid_request_error"),
                Some(keep),
                None,
            ));
            assert!(
                validate_error_messages(&table).is_ok(),
                "承重字符串必须提示不硬拒: {keep}"
            );
        }
        // 非承重文案（含「等容量」）同样放行——它不承载任何 shield 判据。
        let plain = one_error_entry(error_entry(
            Some(400),
            Some("invalid_request_error"),
            Some("上游仍不可用（等容量）。请退避重试。"),
            None,
        ));
        assert!(
            validate_error_messages(&plain).is_ok(),
            "「等容量」不是承重词，含它的文案必须正常放行"
        );
    }

    #[test]
    fn validate_rejects_bad_key_name_and_oversize() {
        // key 命名规范。
        for bad_key in ["QuotaExhausted", "quota exhausted", "1quota", "quota!", ""] {
            let mut table = HashMap::new();
            table.insert(
                bad_key.to_string(),
                error_entry(Some(429), Some("rate_limit_error"), None, None),
            );
            assert!(
                validate_error_messages(&table).is_err(),
                "非法 key 名必须拒绝: {bad_key:?}"
            );
        }
        // message 超长。
        let long_msg = "x".repeat(501);
        let table = one_error_entry(error_entry(
            Some(429),
            Some("rate_limit_error"),
            Some(&long_msg),
            None,
        ));
        assert!(
            validate_error_messages(&table).is_err(),
            "message 超过 500 字符必须拒绝"
        );
        // 表条目数上限（200）。
        let mut big = HashMap::new();
        for i in 0..=ERROR_TABLE_MAX_ENTRIES {
            big.insert(format!("key_{i}"), error_entry(None, None, None, None));
        }
        assert!(
            validate_error_messages(&big).is_err(),
            "超过 {} 条必须拒绝",
            ERROR_TABLE_MAX_ENTRIES
        );
    }

    /// B1：组合校验必须用「配置 or 默认表」的**最终渲染值**——只配 status 或只配
    /// type 时另一半落默认，仍必须过组合矩阵（防单字段绕过）。
    ///
    /// key 不硬编码：默认表可能被并行任务重写（key 集变化），动态从
    /// `default_error_messages()` 取「默认 429+rate_limit_error」的 key——
    /// 改表场景测试自适应（表里没有该基线 key 时显式 panic 提示）。
    #[test]
    fn validate_rendered_combination_rejects_single_field_bypass() {
        let table = crate::model::error_messages::default_error_messages();
        let base = table
            .iter()
            .find(|(_, s, t, ..)| *s == 429 && *t == "rate_limit_error")
            .map(|(k, ..)| k.to_string())
            .expect("默认表必须保留至少一个 429+rate_limit_error 的 key（B1 渲染值组合校验基线）");

        // status-only 绕过：只配 status=401 → 渲染 401 + 默认 rate_limit_error → 拒。
        let mut status_only = HashMap::new();
        status_only.insert(base.clone(), error_entry(Some(401), None, None, None));
        let err = validate_error_messages(&status_only)
            .expect_err("只配 status 必须按渲染值过组合矩阵");
        assert!(err.contains("组合不合法"), "实际: {err}");

        // type-only 绕过：只配 type=authentication_error → 渲染 429 + 该 type → 拒。
        let mut type_only = HashMap::new();
        type_only.insert(base, error_entry(None, Some("authentication_error"), None, None));
        let err = validate_error_messages(&type_only)
            .expect_err("只配 type 必须按渲染值过组合矩阵");
        assert!(err.contains("组合不合法"), "实际: {err}");

        // 双显式合法 → 通过；双显式非法 → 拒。
        let ok = one_error_entry(error_entry(Some(429), Some("rate_limit_error"), None, None));
        assert!(validate_error_messages(&ok).is_ok(), "双显式合法组合必须通过");
        let bad = one_error_entry(error_entry(Some(429), Some("api_error"), None, None));
        assert!(validate_error_messages(&bad).is_err(), "双显式非法组合必须拒绝");
    }

    /// B1 改默认表场景：默认表所有「默认 status/type 都在官方白名单」的 key，其默认
    /// 渲染值必须自身组合合法——否则管理员对该 key 的任何配置（含只改 message 的
    /// 合法姿势）都会被渲染值组合检查误伤。并行任务重写默认表时本测试自动跟随。
    #[test]
    fn validate_default_table_combos_are_self_consistent() {
        let table = crate::model::error_messages::default_error_messages();
        let mut official = 0;
        for &(key, s, t, ..) in table {
            if ERROR_STATUS_WHITELIST.contains(&s) && ERROR_TYPE_WHITELIST.contains(&t) {
                official += 1;
                assert!(
                    error_type_compatible_with_status(s, t),
                    "默认表 {key}: 默认渲染 {s}+{t} 必须组合合法，\
                     否则管理员对该 key 的任何配置都会被渲染值检查拒绝"
                );
            }
        }
        assert!(official > 0, "默认表必须存在官方值域内的 key（否则渲染值检查无靶点）");
    }

    /// m2：504 必须在 status 白名单（`upstream_timeout` 默认 504——管理员显式写回
    /// 默认值时不被拒），组合上归 5xx→api_error 族（H5）。
    #[test]
    fn validate_accepts_504_upstream_timeout_default() {
        let ok = one_error_entry(error_entry(Some(504), Some("api_error"), None, None));
        assert!(validate_error_messages(&ok).is_ok(), "504+api_error 必须合法");
        let bad = one_error_entry(error_entry(Some(504), Some("rate_limit_error"), None, None));
        assert!(
            validate_error_messages(&bad).is_err(),
            "504 组合必须归 api_error 族"
        );
    }

    /// B2：billing_error / quota_exceeded_error 已从 type 白名单移除——任何 status
    /// 配置都拒绝（Claude Code CLI 层对 429/402+billing_error 重试约 7 次/1 分钟 =
    /// 重试风暴；quota_exceeded_error 需 402 支持，见白名单注释）；quota+exhausted
    /// 决策词无豁免（豁免条件随 billing_error 移除永远不可达）。
    #[test]
    fn validate_rejects_billing_error_and_quota_exceeded_error() {
        for status in [400u16, 401, 403, 404, 413, 429, 500, 502, 503, 504] {
            let table = one_error_entry(error_entry(Some(status), Some("billing_error"), None, None));
            let err = validate_error_messages(&table)
                .expect_err("非 402 的 billing_error 必须拒绝");
            assert!(
                err.contains(".type") || err.contains("402") || err.contains("组合不合法"),
                "必须点名 type 或不兼容组合，实际: {err}"
            );
        }
        let ok_402 = one_error_entry(error_entry(
            Some(402),
            Some("billing_error"),
            None,
            None,
        ));
        assert!(
            validate_error_messages(&ok_402).is_ok(),
            "402+billing_error 是全池配额出口，必须放行"
        );
        let quota_ty = one_error_entry(error_entry(
            Some(429),
            Some("quota_exceeded_error"),
            None,
            None,
        ));
        assert!(
            validate_error_messages(&quota_ty).is_err(),
            "429+quota_exceeded_error 必须拒绝（只允许 402）"
        );
        let ok_quota_402 = one_error_entry(error_entry(
            Some(402),
            Some("quota_exceeded_error"),
            None,
            None,
        ));
        assert!(validate_error_messages(&ok_quota_402).is_ok());
        // quota+exhausted 决策词：配任何 type（含无 type）都无条件拒。
        for ty in [Some("rate_limit_error"), Some("overloaded_error"), None] {
            let table = one_error_entry(error_entry(
                Some(429),
                ty,
                Some("Monthly quota exhausted"),
                None,
            ));
            assert!(
                validate_error_messages(&table).is_err(),
                "quota+exhausted 必须无条件拒绝 (ty={ty:?})"
            );
        }
    }
}

#[cfg(test)]
mod balance_cache_tests {
    use super::super::*;
    use super::AwaitOk;

    fn make_cached(id: u64, cached_at: f64) -> (String, CachedBalance) {
        (
            id.to_string(),
            CachedBalance {
                cached_at,
                data: BalanceResponse {
                    id,
                    subscription_title: Some("Kiro Pro".to_string()),
                    current_usage: 10.0,
                    usage_limit: 100.0,
                    remaining: 90.0,
                    usage_percentage: 10.0,
                    next_reset_at: None,
                    overage_enabled: false,
                    overage_cap: 0.0,
                    effective_limit: 100.0,
                    stale: false,
                    optimistic: false,
                },
            },
        )
    }

    /// 造一个带单个凭据的 AdminService（余额展示 / 节点池 / 多开测试共用）。
    pub(super) fn mk_service_with_one_credential() -> AdminService {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 造一条余额缓存条目（remaining=90 / limit=100 / used=10）。
    fn mk_cached_balance(id: u64, cached_at: f64) -> CachedBalance {
        CachedBalance {
            cached_at,
            data: BalanceResponse {
                id,
                subscription_title: Some("Kiro Pro".to_string()),
                current_usage: 10.0,
                usage_limit: 100.0,
                remaining: 90.0,
                usage_percentage: 10.0,
                next_reset_at: None,
                overage_enabled: false,
                overage_cap: 0.0,
                effective_limit: 100.0,
                stale: false,
                optimistic: false,
            },
        }
    }

    /// 造一个池：`n` 份**同 key** 的 api_key 号（模拟分身组）+ 一个**不同 key** 的对照号。
    fn mk_service_with_clone_group(n: u64) -> AdminService {
        let mut creds = Vec::new();
        for i in 1..=n {
            let mut c = crate::kiro::model::credentials::KiroCredentials::default();
            c.id = Some(i);
            c.auth_method = Some("api_key".to_string());
            // 同一个 key ⇒ 同一个上游账号 ⇒ 必须共享余额
            c.kiro_api_key = Some("ksk_shared_group".to_string());
            creds.push(c);
        }
        // 对照：不同 key，绝不能与上面那组混成一条
        let mut other = crate::kiro::model::credentials::KiroCredentials::default();
        other.id = Some(n + 1);
        other.auth_method = Some("api_key".to_string());
        other.kiro_api_key = Some("ksk_different".to_string());
        creds.push(other);

        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// ⭐ 回归（dwgx 需求「同一个 key 的分身和凭据余额必须同步」）：
    /// 缓存按**账号**键，一次写入即全组可见。
    ///
    /// # 旧代码为何 FAIL
    ///
    /// `balance_cache` 原先是 `HashMap<u64, _>` 按**凭据 id** 键，于是同一个 `ksk_` key 的
    /// N 份分身各存一份余额 ⇒ 面板上同组各份显示的数字**互不相同**（谁最近刷过谁新），
    /// 而它们描述的本来是同一个上游账号、同一份配额。
    /// 线上实测缓存键是 `620/623/622/624` —— 四份分身四条独立记录。
    ///
    /// # 断言的是可观测状态
    ///
    /// 不断言内部键长什么样（那是实现细节），而是断言 `list_cached_balances` 这个
    /// **前端真正消费的端点**对同组各份返回同一个 `remaining`。
    ///
    /// 把 `balance_cache` 改回按 id 键 → 本测试必 FAILED。
    #[test]
    fn same_api_key_credentials_share_one_balance() {
        let svc = mk_service_with_clone_group(4);

        // 只给**其中一份**写缓存（模拟"任一份刷新过"）
        let now = Utc::now().timestamp() as f64;
        {
            let key = svc.balance_cache_key(2);
            let mut cache = svc.balance_cache.lock();
            cache.insert(key, mk_cached_balance(2, now));
        }

        let resp = svc.get_cached_balances();

        // 同组四份**全部**应拿到余额，且数字一致
        for id in 1..=4u64 {
            let item = resp.balances.get(&id).unwrap_or_else(|| {
                panic!("凭据 #{id} 应共享同组余额（旧代码按 id 键 ⇒ 只有 #2 有值）")
            });
            assert!(
                (item.balance.remaining - 90.0).abs() < 1e-6,
                "凭据 #{id} 的 remaining 应与同组一致，实际 {}",
                item.balance.remaining
            );
        }

        // ⭐ 承重反向断言：**不同 key** 的号绝不能被混进来。
        // 若为了"统一"给所有号一个共享键，面板会显示别人的额度 —— 那比不同步严重得多。
        assert!(
            resp.balances.get(&5).is_none(),
            "不同 key 的凭据 #5 不得共享这条余额（那会显示别的账号的额度）"
        );
    }

    /// ⭐ 回归：旧格式缓存（按凭据 id 键）必须被**迁移**成账号键，而不是静默失效。
    ///
    /// # 不迁移的代价
    ///
    /// 键从 `id` 改成 `sha256(apiKey)` 后，旧文件里的十进制 id 键永远不会被命中 ⇒
    /// 升级后 api_key 号余额全空 ⇒ 面板集体转圈打 `getUsageLimits`。那是 `web_portal`
    /// 上游探测，本仓调优结论是绝不为展示类需求反复打它。
    ///
    /// 实测规模：线上 5 条缓存 / 5 个 api_key 号 / **只有 1 个不同的 key** ⇒ 并成 1 条。
    ///
    /// # 并组取最新
    ///
    /// N 个 id 映射到同一账号键时按 `cached_at` 取最新 —— 旧的那些本来就是冗余副本。
    ///
    /// 把迁移改回"键原样保留" → 本测试必 FAILED。
    #[test]
    fn old_id_keyed_cache_migrates_to_account_key() {
        let dir = std::env::temp_dir().join(format!("kiro_bal_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kiro_balance_cache.json");

        let now = Utc::now().timestamp() as f64;
        // 旧格式：三份同 key 分身各一条，cached_at 递增（#3 最新）
        let mut map: HashMap<String, CachedBalance> = HashMap::new();
        for (id, age) in [(1u64, 300.0), (2u64, 200.0), (3u64, 100.0)] {
            let mut cb = mk_cached_balance(id, now - age);
            // 用 remaining 标记是哪条，便于断言"取到的是最新那条"
            cb.data.remaining = 90.0 - age;
            map.insert(id.to_string(), cb);
        }
        std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();

        // 池里是三份同 key 的 api_key 号（mk_service_with_clone_group 的构造）
        let svc = mk_service_with_clone_group(3);
        let loaded = AdminService::load_balance_cache_from(&Some(path.clone()), &svc.token_manager);

        // 三条旧键并成一条账号键
        assert_eq!(
            loaded.len(),
            1,
            "三份同 key 分身的旧缓存应并成 1 条账号键，实际 {} 条：{:?}",
            loaded.len(),
            loaded.keys().collect::<Vec<_>>()
        );
        let account_key = svc.balance_cache_key(1);
        let kept = loaded
            .get(&account_key)
            .expect("并组后的键应等于 balance_cache_key 算出的账号键");
        // 取的是 cached_at 最新那条（age=100 ⇒ remaining = 90-100 = -10）
        assert!(
            (kept.data.remaining - (-10.0)).abs() < 1e-6,
            "并组应保留 cached_at 最新的那条，实际 remaining={}",
            kept.data.remaining
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 回归：删掉一份分身**不得**清掉整组共享的余额缓存。
    ///
    /// 无条件 `remove` 会让「删一份」把全组缓存清空 ⇒ 剩下的份面板显示"暂无数据"，
    /// 直到下次后台刷新（默认 30 分钟）。而键必须在删除**之前**算 —— 删掉后
    /// `export_credential` 返 `None`、键回落成 id 字符串，清的是不存在的键。
    ///
    /// 把 `prune_balance_cache_for_deleted` 改回无条件 remove → 本测试必 FAILED。
    #[test]
    fn deleting_one_clone_keeps_group_balance_cache() {
        let svc = mk_service_with_clone_group(3);
        let now = Utc::now().timestamp() as f64;
        let group_key = svc.balance_cache_key(1);
        {
            let mut cache = svc.balance_cache.lock();
            cache.insert(group_key.clone(), mk_cached_balance(1, now));
        }

        // 删掉组内一份（force 跳过"必须先禁用"那道门）
        svc.delete_credential_forced(2, true).expect("删除应成功");

        assert!(
            svc.balance_cache.lock().contains_key(&group_key),
            "删一份分身后，整组共享的余额缓存必须仍在（同 key 的其余份还要用它）"
        );
        // 剩下的份仍能读到
        let resp = svc.get_cached_balances();
        assert!(
            resp.balances.contains_key(&1) && resp.balances.contains_key(&3),
            "剩余份应仍有余额可显示"
        );
    }

    /// 回归（dwgx 需求「用了余额之后要刷新额度显示」）：展示路径必须用本地累计花费做乐观修正。
    ///
    /// **旧代码为何 FAIL**：余额真值由后台每 30 分钟刷新一次，展示端点原样吐缓存 →
    /// 跑完一批请求后额度**最多 30 分钟不动**，用户以为没生效。
    /// 本测试推进 `total_credits_used` 而不刷新缓存，断言展示值已跟着走。
    ///
    /// 关键约束：**绝不为此每请求打上游** —— 那是 web_portal 探测会加重风控
    /// （线上号池正被风控烧号）。所以修正只用已有的两份内存数据（累计花费 + 缓存基线）。
    #[test]
    fn cached_balances_apply_optimistic_credit_adjustment() {
        let svc = mk_service_with_one_credential();
        // 播种：缓存里有真值（remaining=90），基线 credits_used=0
        // 键走 balance_cache_key（缓存已改为按**账号**键，不再是凭据 id）。
        let k = svc.balance_cache_key(1);
        svc.balance_cache
            .lock()
            .insert(k, mk_cached_balance(1, Utc::now().timestamp() as f64));
        svc.token_manager.set_balance_snapshots(HashMap::from([(
            1u64,
            crate::kiro::token_manager::BalanceSnapshot {
                remaining_at_cache: 90.0,
                effective_limit: 100.0,
                credits_used_at_cache: 0.0,
            },
        )]));

        // 未花钱时：展示值 = 真值，且不标 optimistic
        let before = svc.get_cached_balances();
        let b0 = &before.balances.get(&1).expect("应有缓存条目").balance;
        assert_eq!(b0.remaining, 90.0);
        assert!(!b0.optimistic, "未花钱不应标记乐观修正");

        // 花掉 5 个 credit（模拟请求完成后 meteringEvent 累加），**不**刷新余额缓存
        svc.token_manager.add_credits(1, 5.0);

        let after = svc.get_cached_balances();
        let b1 = &after.balances.get(&1).expect("应有缓存条目").balance;
        assert_eq!(
            b1.remaining, 85.0,
            "remaining 未跟随本地花费推进（旧代码原样吐缓存，30 分钟内恒为 90）"
        );
        assert_eq!(b1.current_usage, 15.0, "current_usage 应同步推进");
        assert!(b1.optimistic, "含本地推算的值必须标记 optimistic，供前端区分真值");
    }

    /// 回归：乐观修正**只单向推进**，且 remaining 不得为负。
    ///
    /// 基线可能比当前累计值更大（重启后 total_credits_used 从 0 重新累计），
    /// 此时 delta<0，绝不能把额度往回加 —— 那会显示出"用了反而变多"。
    #[test]
    fn optimistic_adjustment_is_monotonic_and_clamped() {
        let svc = mk_service_with_one_credential();
        // 键走 balance_cache_key（缓存已改为按**账号**键）。
        let k = svc.balance_cache_key(1);
        svc.balance_cache
            .lock()
            .insert(k, mk_cached_balance(1, Utc::now().timestamp() as f64));
        // 基线 999：远大于当前累计（0），delta 为负
        svc.token_manager.set_balance_snapshots(HashMap::from([(
            1u64,
            crate::kiro::token_manager::BalanceSnapshot {
                remaining_at_cache: 90.0,
                effective_limit: 100.0,
                credits_used_at_cache: 999.0,
            },
        )]));
        let r = svc.get_cached_balances();
        let b = &r.balances.get(&1).unwrap().balance;
        assert_eq!(b.remaining, 90.0, "delta<=0 时不得改动展示值（不能出现'用了反而变多'）");
        assert!(!b.optimistic);

        // 花超额度：remaining 收敛到 0 而非负数
        svc.token_manager.set_balance_snapshots(HashMap::from([(
            1u64,
            crate::kiro::token_manager::BalanceSnapshot {
                remaining_at_cache: 90.0,
                effective_limit: 100.0,
                credits_used_at_cache: 0.0,
            },
        )]));
        svc.token_manager.add_credits(1, 500.0);
        let r2 = svc.get_cached_balances();
        let b2 = &r2.balances.get(&1).unwrap().balance;
        assert_eq!(b2.remaining, 0.0, "remaining 不得为负");
        assert!(b2.usage_percentage <= 100.0, "百分比不得超 100");
    }

    /// 回归测试：启动恢复必须保留“陈旧但仍在展示保留期内”的余额缓存，
    /// 而不是用 5 分钟新鲜度阈值把它整批丢成“未知”（这正是重启后余额消失的根因）。
    #[test]
    fn load_keeps_stale_but_within_display_window() {
        let dir = std::env::temp_dir().join(format!("ks_bal_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kiro_balance_cache.json");

        let now = Utc::now().timestamp() as f64;
        // 1 小时前写入：远超 5 分钟新鲜度阈值，但远在 7 天展示保留期内
        let stale = now - 3600.0;
        // 8 天前写入：超过展示保留期，应被丢弃
        let ancient = now - (8.0 * 24.0 * 3600.0);

        let mut map: HashMap<String, CachedBalance> = HashMap::new();
        let (k1, v1) = make_cached(1, stale);
        let (k2, v2) = make_cached(2, ancient);
        map.insert(k1, v1);
        map.insert(k2, v2);
        std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();

        // 传一个空池的 token_manager：本测试只验「7 天展示保留期」的淘汰，
        // 不验账号键迁移（那条由 migration 专用测试覆盖）。空池 ⇒ 键原样保留。
        let tm_empty = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![],
                None,
                None,
                false,
            )
            .expect("构造空池 token manager"),
        );
        let loaded = AdminService::load_balance_cache_from(&Some(path.clone()), &tm_empty);

        // 键现在**原样保留为字符串**（缓存改按账号键后不再 parse 成 u64）。
        // 磁盘格式不变（JSON 对象键本来就是字符串），所以旧文件仍能读回。
        // 陈旧但在展示窗口内 → 保留（重启后前端仍能显示上次数字）
        assert!(loaded.contains_key("1"), "陈旧但在 7 天内的缓存必须保留");
        // 超过展示窗口 → 丢弃（避免无界陈旧）
        assert!(!loaded.contains_key("2"), "超过 7 天的缓存应被丢弃");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 源码守卫：余额缓存簇在 `balance_cache.rs` sibling，父 `service.rs` 保持文件。
    ///
    /// 回退即 FAIL：去掉 `#[path]`、或把 `save`/`load` 定义写回父文件。
    /// `new()` 里 `Self::load_balance_cache_from` 调用点必须仍在父文件。
    #[test]
    fn service_rs_delegates_balance_cache_to_path_sibling() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let path_attr = format!("{}{}", "#[path = \"balance_cache.", "rs\"]");
        assert!(
            prod.contains(path_attr.as_str()),
            "production service.rs 必须 #[path] 接入 balance_cache.rs（父文件保持为文件）"
        );
        let save_def = format!("{}{}", "fn save_balance", "_cache");
        let load_def = format!("{}{}", "fn load_balance_cache", "_from");
        assert!(
            !prod.contains(save_def.as_str()),
            "save_balance_cache 定义必须在 sibling，父文件只允许调用点"
        );
        assert!(
            !prod.contains(load_def.as_str()),
            "load_balance_cache_from 定义必须在 sibling；new() 里 Self::load_balance_cache_from 调用点保留"
        );
        let load_call = format!("{}{}", "Self::load_balance_cache", "_from(");
        assert!(
            prod.contains(load_call.as_str()),
            "AdminService::new 必须仍调用 load_balance_cache_from 构造缓存"
        );
    }

    /// 源码守卫：socks 池在 `socks_nodes.rs` sibling，父 `service.rs` 保持文件。
    ///
    /// 回退即 FAIL：去掉 `#[path]`、或把 `persist`/`load` 定义写回父文件。
    /// `new()` 里 `Self::load_socks_nodes_from` 调用点必须仍在父文件。
    #[test]
    fn service_rs_delegates_socks_nodes_to_path_sibling() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let path_attr = format!("{}{}", "#[path = \"socks_nodes.", "rs\"]");
        assert!(
            prod.contains(path_attr.as_str()),
            "production service.rs 必须 #[path] 接入 socks_nodes.rs（父文件保持为文件）"
        );
        let persist_def = format!("{}{}", "fn persist_socks", "_nodes");
        let load_def = format!("{}{}", "fn load_socks_nodes", "_from");
        assert!(
            !prod.contains(persist_def.as_str()),
            "persist_socks_nodes 定义必须在 sibling，父文件只允许调用点"
        );
        assert!(
            !prod.contains(load_def.as_str()),
            "load_socks_nodes_from 定义必须在 sibling；new() 里 Self::load_socks_nodes_from 调用点保留"
        );
        let load_call = format!("{}{}", "Self::load_socks_nodes", "_from(");
        assert!(
            prod.contains(load_call.as_str()),
            "AdminService::new 必须仍调用 load_socks_nodes_from 构造节点表"
        );
    }

    /// 源码守卫：配置更新簇在 `config_update.rs` sibling，父 `service.rs` 保持文件。
    ///
    /// 回退即 FAIL：去掉 `#[path]`、或把 `update_config_locked` 定义写回父文件。
    /// `import_config` 仍在父文件（可调 `Self::update_config_locked`）。
    #[test]
    fn service_rs_delegates_update_config_to_path_sibling() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let path_attr = format!("{}{}", "#[path = \"config_update.", "rs\"]");
        assert!(
            prod.contains(path_attr.as_str()),
            "production service.rs 必须 #[path] 接入 config_update.rs（父文件保持为文件）"
        );
        let locked_def = format!("{}{}", "fn update_config", "_locked");
        assert!(
            !prod.contains(locked_def.as_str()),
            "update_config_locked 定义必须在 sibling，父文件只允许调用点"
        );
    }

    /// 源码守卫：自重启簇在 `service_restart.rs` sibling，父 `service.rs` 保持文件。
    ///
    /// 回退即 FAIL：去掉 `#[path]`、或把 `restart_service` 定义写回父文件。
    /// 托盘仍走 `admin::spawn_windows_relaunch_process`（父文件 re-export）。
    #[test]
    fn service_rs_delegates_restart_service_to_path_sibling() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let path_attr = format!("{}{}", "#[path = \"service_restart.", "rs\"]");
        assert!(
            prod.contains(path_attr.as_str()),
            "production service.rs 必须 #[path] 接入 service_restart.rs（父文件保持为文件）"
        );
        let restart_def = format!("{}{}", "fn restart", "_service");
        assert!(
            !prod.contains(restart_def.as_str()),
            "restart_service 定义必须在 sibling，父文件只允许调用点"
        );
    }

    fn upsert_req(id: Option<u64>, url: &str, password: Option<&str>) -> SocksNodeUpsertRequest {
        SocksNodeUpsertRequest {
            id,
            name: Some("n".into()),
            url: url.into(),
            username: Some("u".into()),
            password: password.map(|s| s.to_string()),
            enabled: None,
        }
    }

    /// ⭐ 承重：**省略 `password` 键 = 不改密码**。
    ///
    /// 回退即 FAIL：把 upsert 里那个 `match req.password` 换成无条件
    /// `node.password = req.password` → 改个节点名就把密码抹成 None，
    /// 已绑该节点的分身在下次请求时全部因代理认证失败而掉线。
    #[tokio::test]
    async fn omitted_password_keeps_existing() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(upsert_req(
                None,
                "socks5://node.invalid:40002",
                Some("secret"),
            ))
            .await
            .expect("新建节点");
        assert_eq!(
            svc.socks_node_proxy(id).and_then(|(_, _, p)| p).as_deref(),
            Some("secret")
        );

        // 只改名，**不带** password 键。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(id),
            name: Some("renamed".into()),
            url: "socks5://node.invalid:40002".into(),
            username: Some("u".into()),
            password: None,
            enabled: None,
        })
        .await
        .expect("更新节点");

        assert_eq!(
            svc.socks_node_proxy(id).and_then(|(_, _, p)| p).as_deref(),
            Some("secret"),
            "省略 password 键必须保留原密码"
        );
    }

    /// `password: ""` 才是清空。
    #[tokio::test]
    async fn empty_password_clears() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(upsert_req(
                None,
                "socks5://node.invalid:40002",
                Some("secret"),
            ))
            .await
            .unwrap();
        svc.upsert_socks_node(upsert_req(
            Some(id),
            "socks5://node.invalid:40002",
            Some(""),
        ))
        .await
        .unwrap();
        assert!(
            svc.socks_node_proxy(id).and_then(|(_, _, p)| p).is_none(),
            "显式空字符串必须清空密码"
        );
    }

    /// 列表视图**绝不外传密码**，只给 hasPassword。
    #[tokio::test]
    async fn list_never_leaks_password() {
        let svc = mk_service_with_one_credential();
        svc.upsert_socks_node(upsert_req(
            None,
            "socks5://node.invalid:40002",
            Some("secret"),
        ))
        .await
        .unwrap();
        let view = svc.list_socks_nodes();
        assert_eq!(view.len(), 1);
        assert!(view[0].has_password, "应报告设了密码");
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("secret"), "序列化后绝不能含密码明文: {json}");
    }

    /// 更新一个**不存在**的 id 必须 404，不得静默新建。
    #[tokio::test]
    async fn upsert_unknown_id_is_not_found() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .upsert_socks_node(upsert_req(Some(999), "socks5://node.invalid:40002", None))
            .await
            .expect_err("不存在的 id 应报错");
        assert!(
            matches!(err, AdminServiceError::NotFound { id: 999 }),
            "应是 NotFound，实际 {err:?}"
        );
        assert!(svc.list_socks_nodes().is_empty(), "不得静默新建");
    }

    /// 内网 IP 字面量的节点地址必须被拒（只覆盖字面量，见 validate_proxy_address 文档）。
    ///
    /// 用**云元数据地址**（169.254.169.254）而不是 127.0.0.1 做样本：节点地址走
    /// `SsrfPolicy::AdminConfigured`，而链路本地段是它明确不豁免的（唯一豁免的是
    /// 198.18.0.0/15 fake-IP 池段，见下方第二条断言）。挑一个策略切换后语义仍然
    /// 明确的地址，测试才不会随策略调整而变成「碰巧还过」。
    #[tokio::test]
    async fn internal_node_address_is_rejected() {
        let svc = mk_service_with_one_credential();
        let err = svc
            .upsert_socks_node(upsert_req(None, "socks5://169.254.169.254:1080", None))
            .await
            .expect_err("云元数据链路本地地址应被拒");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        assert!(svc.list_socks_nodes().is_empty());

        // ⭐ 承重：fake-IP 池段必须能加进来（这才是 AdminConfigured 的目的）。
        // 回退即 FAIL：把 validate_proxy_address 的策略改回 Strict —— 开了 Clash
        // fake-IP 的机器上任意域名都解析到该段，节点池对这些用户完全不可用。
        svc.upsert_socks_node(upsert_req(None, "socks5://198.18.0.46:40002", None))
            .await
            .expect("fake-IP 池段（198.18.0.0/15）在 AdminConfigured 下必须放行");
        assert_eq!(svc.list_socks_nodes().len(), 1);
    }

    /// 删节点**不动**已绑该节点的凭据（删一个节点不该让一批分身掉线）。
    #[tokio::test]
    async fn deleting_node_leaves_credential_proxy_untouched() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(upsert_req(None, "socks5://node.invalid:40002", Some("p")))
            .await
            .unwrap();
        // 把节点地址绑到凭据上（模拟「生成分身时写进凭据」）。
        svc.token_manager
            .set_credential_proxy(
                1,
                Some("socks5://node.invalid:40002".into()),
                Some("u".into()),
                Some("p".into()),
            )
            .expect("绑定代理");

        assert!(svc.delete_socks_node(id).unwrap());

        let cred = svc.token_manager.export_credential(1).expect("凭据仍在");
        assert_eq!(
            cred.proxy_url.as_deref(),
            Some("socks5://node.invalid:40002"),
            "删节点不得清掉凭据上已生效的代理绑定"
        );
    }

    /// ⭐ 最重要的一条：**文件在但读不出来时，绝不能把它覆盖掉**。
    ///
    /// 回退即 FAIL：把 `load_socks_nodes_from` 的解析失败分支改回
    /// `(Vec::new(), 1, true)`（即「空表 + 允许回写」），或删掉
    /// `persist_socks_nodes` 里那道 `socks_nodes_writable` 判断 —— 两者任一都会让
    /// 下面最后那条断言失败：原文件里的节点与代理密码被一张只有 1 条的表原子覆盖，
    /// 永久丢失。这是把 credentials.json 那条 `exit(1)` 换成只读降级的代价，
    /// 必须有测试兜住。
    #[test]
    fn unreadable_node_file_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_ro_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socks_nodes.json");

        // 写一份**读不出来**的内容（既非合法 JSON 也非 KSENC1 密文）。
        let garbage = b"{ this is not valid json at all";
        std::fs::write(&path, garbage).unwrap();

        let (nodes, next_id, writable) =
            AdminService::load_socks_nodes_from(&Some(path.clone()), &{
                let mut c = crate::kiro::model::credentials::KiroCredentials::default();
                c.id = Some(1);
                c.auth_method = Some("api_key".into());
                c.kiro_api_key = Some("ksk_ro".into());
                Arc::new(
                    MultiTokenManager::new(
                        crate::model::config::Config::default(),
                        vec![c],
                        None,
                        None,
                        false,
                    )
                    .expect("token manager"),
                )
            });

        assert!(nodes.is_empty(), "读不出来时内存表应为空");
        assert_eq!(next_id, 1);
        assert!(
            !writable,
            "文件存在但解析失败必须进入只读降级，否则下一次修改会抹平它"
        );

        // ⭐ 承重：真的走一遍**写路径**，再核对磁盘。
        //
        // 只调 loader 是不够的（本测试第一版就只做了这一半）：那样删掉
        // `persist_socks_nodes` 里的 writable 判断，测试**照样通过** ——
        // 因为它从没写过。必须构造一个 socks_nodes_path 指向该文件、
        // socks_nodes_writable=false 的 service，然后调 persist 并断言两件事：
        // 调用被拒 + 文件逐字节未变。
        let svc = AdminService {
            socks_nodes: Mutex::new(vec![SocksNode {
                id: 1,
                name: "n".into(),
                url: "socks5://node.invalid:40002".into(),
                username: None,
                password: Some("would-be-written".into()),
                enabled: true,
                last_test: None,
                created_at: 0,
            }]),
            socks_nodes_path: Some(path.clone()),
            socks_nodes_writable: writable, // = false
            ..AdminService::new(
                {
                    let mut c = crate::kiro::model::credentials::KiroCredentials::default();
                    c.id = Some(1);
                    c.auth_method = Some("api_key".into());
                    c.kiro_api_key = Some("ksk_ro2".into());
                    Arc::new(
                        MultiTokenManager::new(
                            crate::model::config::Config::default(),
                            vec![c],
                            None,
                            None,
                            false,
                        )
                        .expect("token manager"),
                    )
                },
                Vec::<String>::new(),
            )
        };
        let err = svc
            .persist_socks_nodes()
            .expect_err("只读降级下回写必须被拒绝");
        assert!(
            matches!(err, AdminServiceError::InternalError(_)),
            "应是 InternalError，实际 {err:?}"
        );

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            after, garbage,
            "只读降级下原文件必须逐字节保持不变（这是防数据毁灭的唯一护栏）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 承重：只读降级下的 upsert **不得改内存**。
    ///
    /// 回退即 FAIL：把 `upsert_socks_node` 顶部那句 `self.ensure_socks_writable()?` 删掉
    /// （即回到「先 push 进内存、再由 persist 报错」的顺序）—— 下面第 2 条断言失败：
    /// 调用方收到报错、磁盘上什么都没有，但 `list_socks_nodes()` 里凭空多出一个节点，
    /// 面板会一直显示它直到重启。
    #[tokio::test]
    async fn readonly_degraded_upsert_leaves_memory_untouched() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_ro_upsert_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socks_nodes.json");
        let garbage = b"{ not json";
        std::fs::write(&path, garbage).unwrap();

        let svc = AdminService {
            socks_nodes: Mutex::new(Vec::new()),
            socks_nodes_path: Some(path.clone()),
            socks_nodes_writable: false,
            ..mk_service_with_one_credential()
        };

        let err = svc
            .upsert_socks_node(upsert_req(None, "socks5://node.invalid:40002", Some("p")))
            .await
            .expect_err("只读降级下新增节点必须报错");
        assert!(
            matches!(err, AdminServiceError::InternalError(_)),
            "应是 InternalError，实际 {err:?}"
        );
        assert!(
            svc.list_socks_nodes().is_empty(),
            "只读降级下报错后内存表必须仍为空，否则面板显示一个磁盘上不存在的节点"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            garbage,
            "原文件必须逐字节未变"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 承重：只读降级下的 delete / record_test 同样不得改内存。
    ///
    /// 回退即 FAIL：删掉这两个方法顶部的 `ensure_socks_writable()?` —— 删除会让节点
    /// 从面板消失（磁盘上还在），测速结果会写进一张永不落盘的表，两者都是「报错了但
    /// 界面显示已生效」。
    #[test]
    fn readonly_degraded_delete_and_test_leave_memory_untouched() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_ro_del_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socks_nodes.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let svc = AdminService {
            socks_nodes: Mutex::new(vec![SocksNode {
                id: 7,
                name: "n".into(),
                url: "socks5://node.invalid:40002".into(),
                username: None,
                password: None,
                enabled: true,
                last_test: None,
                created_at: 0,
            }]),
            socks_nodes_path: Some(path.clone()),
            socks_nodes_writable: false,
            ..mk_service_with_one_credential()
        };

        assert!(svc.delete_socks_node(7).is_err(), "只读降级下删除必须报错");
        assert_eq!(
            svc.list_socks_nodes().len(),
            1,
            "报错后节点必须还在内存表里（否则面板上它消失了而磁盘上还在）"
        );

        assert!(
            svc.record_socks_node_test(
                7,
                SocksNodeTest {
                    ok: true,
                    latency_ms: 12,
                    error: None,
                    tested_at: 0,
                    exit_ip: None,
                }
            )
            .is_err(),
            "只读降级下写测速结果必须报错"
        );
        assert!(
            svc.list_socks_nodes()[0].last_test.is_none(),
            "报错后不得留下一个永不落盘的测速结果"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缺失文件与不可读文件必须走不同分支：缺失是首次启动（可写），不可读是降级（只读）。
    #[test]
    fn missing_node_file_is_writable_unlike_unreadable_one() {
        let dir = std::env::temp_dir().join(format!(
            "ks_socks_missing_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tm = {
            let mut c = crate::kiro::model::credentials::KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("api_key".into());
            c.kiro_api_key = Some("ksk_missing".into());
            Arc::new(
                MultiTokenManager::new(
                    crate::model::config::Config::default(),
                    vec![c],
                    None,
                    None,
                    false,
                )
                .expect("token manager"),
            )
        };
        let (nodes, next_id, writable) =
            AdminService::load_socks_nodes_from(&Some(dir.join("socks_nodes.json")), &tm);
        assert!(nodes.is_empty());
        assert_eq!(next_id, 1, "首次启动的 next_id 应为 1");
        assert!(writable, "文件不存在是首次启动，必须允许回写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 更新只发 `{id, url, enabled}` 时，用户名与密码都必须保留。
    ///
    /// 回退即 FAIL：把 username 改回无条件 `node.username = username` ——
    /// 用户名被抹成 None 而密码留着，`build_client` 的
    /// `if let (Some(u), Some(p))` 不成立 → 认证被静默丢弃 → 该节点全部连不上，
    /// 而面板上它看起来一切正常（仍显示「已设密码」）。
    #[tokio::test]
    async fn partial_update_preserves_both_username_and_password() {
        let svc = mk_service_with_one_credential();
        let id = svc
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("n".into()),
                url: "socks5://node.invalid:40002".into(),
                username: Some("alice".into()),
                password: Some("secret".into()),
                enabled: None,
            })
            .await
            .expect("新建");

        // 只改 enabled，username/password 两个键都不带。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(id),
            name: None,
            url: "socks5://node.invalid:40002".into(),
            username: None,
            password: None,
            enabled: Some(false),
        })
        .await
        .expect("局部更新");

        let (_, user, pass) = svc.socks_node_proxy(id).expect("节点仍在");
        assert_eq!(
            user.as_deref(),
            Some("alice"),
            "省略 username 键必须保留原值"
        );
        assert_eq!(
            pass.as_deref(),
            Some("secret"),
            "省略 password 键必须保留原值"
        );

        // 显式空串仍必须清空（否则「清除用户名」这个操作不存在）。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(id),
            name: None,
            url: "socks5://node.invalid:40002".into(),
            username: Some(String::new()),
            password: None,
            enabled: None,
        })
        .await
        .expect("清空用户名");
        let (_, user, pass) = svc.socks_node_proxy(id).unwrap();
        assert!(user.is_none(), "显式空串必须清空 username");
        assert_eq!(
            pass.as_deref(),
            Some("secret"),
            "清 username 不该动 password"
        );
    }

    /// ⭐ 源码级守卫：多开必须**消费**节点池，且不得复用节点。
    ///
    /// 用源码断言而非行为测试：`add_credential` 会调 `get_usage_limits_for`
    /// （真实上游往返），穿它的行为测试写不了 —— 本仓既有惯例，见
    /// `provider.rs` 的 `should_emit_usage_record_in_mcp_success_branch`。
    ///
    /// 回退即 FAIL：删掉 copies 循环里那段 `assignable.get(...)` 赋值 ——
    /// 节点池就再次变成一张没人读的表：用户加了节点、建了分身，每份仍然直连、
    /// 共用服务器同一个出口 IP，而面板上看起来一切正常。这正是本批第一版的状态。
    #[test]
    fn clone_creation_must_consume_the_node_pool_without_reuse() {
        let src = include_str!("service.rs");
        // needle 运行时拼接，避免被 include_str! 读到自己而多算一处。
        let consume = format!("{}{}", "assignable.get(seq as usize", " - 2)");
        assert!(
            src.contains(consume.as_str()),
            "多开循环必须按份从节点池取节点，否则节点池无任何消费方"
        );
        // 只取启用节点。
        let enabled_filter = format!("{}{}", ".filter(|n| n.", "enabled)");
        assert!(
            src.contains(enabled_filter.as_str()),
            "只能分配 enabled 的节点，否则「禁用节点」这个开关没有意义"
        );
        // ⭐ 承重：索引式取用（取完即止）而不是取模复用。
        // needle 必须运行时拼接 —— 写成完整字面量时它会出现在 include_str! 读到的
        // 本测试自身里，于是这条**否定**断言恒失败（本文件已两次踩到同一个坑）。
        let reuse = format!("{}{}", "assignable[seq as usize", " % ");
        assert!(
            !src.contains(reuse.as_str()),
            "不得对节点取模复用：两份共用一个出口 IP 等于没分散，却让人以为分散了"
        );
    }

    // ===================== 节点表落盘路径（round-trip）=====================
    //
    // 上面 11 条节点测试全部用 `mk_service_with_one_credential()`，它给
    // `MultiTokenManager::new` 传的 credentials_path 是 `None` → `cache_dir()` 为 None
    // → `socks_nodes_path` 为 None → `persist_socks_nodes` 在开头就 `return Ok(())`。
    // 也就是说**它们一次都没真的写过盘**，于是以下四件事此前零覆盖：
    // 密码的 at-rest 加解密往返、`next_id` 高水位跨存取存活、`SocksNodeFileCompat`
    // 的裸数组兼容分支（生产上唯一引用点是 `load_socks_nodes_from`，测试侧此前为零）、
    // 明文↔密文开关。
    //
    // 下面这组测试建一个 credentials_path 落在**独立临时目录**里的 service，
    // 从而让 `cache_dir()` 派生出真实的 socks_nodes_path（刻意走真实派生链，
    // 而不是直接塞 socks_nodes_path 字段 —— 后者测不到派生本身）。

    /// 造一个节点表真的落在 `dir` 里的 service。`encrypt` 控制 at-rest 开关。
    fn mk_service_rooted_at(dir: &std::path::Path, encrypt: bool) -> AdminService {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_rt".to_string());
        let mut cfg = crate::model::config::Config::default();
        cfg.encrypt_credentials_at_rest = encrypt;
        let tm = Arc::new(
            MultiTokenManager::new(cfg, vec![c], None, Some(dir.join("credentials.json")), true)
                .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 每条测试独立临时目录（密钥文件 `.at_rest.key` 也落在里面，互不污染）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ks_socks_rt_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// (a) 明文落盘往返：字段逐个还原，**含密码**。
    ///
    /// 回退即 FAIL：把 `persist_socks_nodes` 里那句 `write_atomic` 删掉（或让它写
    /// `nodes` 而不带 `next_id`，见下一条）—— 重启后节点表整张消失，
    /// 用户配好的一池代理与密码全部丢失，而面板只会显示「暂无节点」。
    ///
    /// ⚠️ 必须 `multi_thread`：`MultiTokenManager::new` 带真实 credentials_path 时会
    /// 回写凭据文件，而 `persist_credentials` 在 runtime 内走 `block_in_place`
    /// （current_thread runtime 上直接 panic）。本组其余落盘测试同理。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nodes_round_trip_plaintext_preserves_every_field() {
        let dir = tmp_dir("plain");
        let svc = mk_service_rooted_at(&dir, false);
        let id = svc
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("JP-1".into()),
                url: "socks5://node.invalid:40002".into(),
                username: Some("alice".into()),
                password: Some("p@ss-w0rd".into()),
                enabled: Some(true),
            })
            .await
            .expect("新建节点");

        let path = dir.join("socks_nodes.json");
        assert!(path.exists(), "cache_dir 派生的节点表必须真的落盘");
        // 关了加密 → 磁盘上是明文（这一条同时锁住「开关真的有效」）。
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !crate::common::secret_store::is_encrypted(&raw),
            "encrypt_credentials_at_rest=false 时不得写成密文"
        );

        // 模拟重启：从同一路径重新加载。
        let svc2 = mk_service_rooted_at(&dir, false);
        let nodes = svc2.list_socks_nodes();
        assert_eq!(nodes.len(), 1, "重启后节点必须还在");
        assert_eq!(nodes[0].id, id);
        assert_eq!(nodes[0].label, "JP-1");
        assert_eq!(nodes[0].url, "socks5://node.invalid:40002");
        assert!(nodes[0].enabled);
        assert!(nodes[0].has_password);
        let (url, user, pass) = svc2.socks_node_proxy(id).expect("节点仍在");
        assert_eq!(url, "socks5://node.invalid:40002");
        assert_eq!(user.as_deref(), Some("alice"), "用户名必须随文件存活");
        assert_eq!(
            pass.as_deref(),
            Some("p@ss-w0rd"),
            "密码必须随文件存活，否则重启后该节点全部连不上"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (b) 加密落盘往返：磁盘字节**不得含密码明文**，但加载后密码完好。
    ///
    /// 回退即 FAIL：把 `persist_socks_nodes` 里的 `encode_for_disk(..., enc, ...)`
    /// 改成 `encode_for_disk(..., false, ...)`（即忽略 at-rest 开关）——
    /// 第 2 条断言失败：代理密码明文躺在磁盘上，而面板的 at-rest 健康灯仍然是绿的。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nodes_round_trip_encrypted_hides_password_on_disk() {
        let dir = tmp_dir("enc");
        let svc = mk_service_rooted_at(&dir, true);
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: None,
            name: Some("enc".into()),
            url: "socks5://node.invalid:40002".into(),
            username: Some("bob".into()),
            password: Some("super-secret-pw".into()),
            enabled: None,
        })
        .await
        .expect("新建节点");

        let path = dir.join("socks_nodes.json");
        let raw = std::fs::read(&path).unwrap();
        assert!(
            crate::common::secret_store::is_encrypted(&raw),
            "开了 at-rest 时节点表必须带 KSENC1 magic 前缀"
        );
        let needle = b"super-secret-pw";
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "磁盘字节里绝不能出现代理密码明文"
        );
        assert!(
            dir.join(".at_rest.key").exists(),
            "首次加密应在同目录创建密钥文件"
        );

        // 重启后必须能解开（同目录密钥在）。
        let svc2 = mk_service_rooted_at(&dir, true);
        let nodes = svc2.list_socks_nodes();
        assert_eq!(nodes.len(), 1, "密文必须能被解开并加载");
        let (_, user, pass) = svc2.socks_node_proxy(nodes[0].id).expect("节点仍在");
        assert_eq!(user.as_deref(), Some("bob"));
        assert_eq!(pass.as_deref(), Some("super-secret-pw"), "解密后密码应完好");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (c) `next_id` 高水位**跨重启**存活：删掉最大 id 的节点后重启，新号仍更大。
    ///
    /// 这是 `SocksNodeFile` 存在的全部理由（见其文档），而此前没有任何测试真的
    /// 存过一次盘 —— 于是"高水位被持久化"这件事从未被验证过。
    ///
    /// 回退即 FAIL：把 `persist_socks_nodes` 里的 `SocksNodeFile { nodes, next_id }`
    /// 换成直接序列化 `nodes` 裸数组（即回到"只存数组"）—— 重启后 next_id 只能按
    /// `max(id)+1` 现算，而最大那个刚被删掉，于是它的 id 被重新发出去：
    /// 面板另一个标签页仍持有删除前的列表，点它的「测活」会打到这个无关的新节点上。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_id_high_water_mark_survives_save_and_load() {
        let dir = tmp_dir("hwm");
        let svc = mk_service_rooted_at(&dir, false);
        let mk = |n: u16| SocksNodeUpsertRequest {
            id: None,
            name: Some(format!("n{n}")),
            url: format!("socks5://node{n}.invalid:40002"),
            username: None,
            password: None,
            enabled: None,
        };
        let a = svc.upsert_socks_node(mk(1)).await.unwrap();
        let b = svc.upsert_socks_node(mk(2)).await.unwrap();
        let c = svc.upsert_socks_node(mk(3)).await.unwrap();
        assert!(c > b && b > a);

        // 删掉**最大** id 那个（这正是"只存数组"会翻车的场景）。
        assert!(svc.delete_socks_node(c).unwrap());

        // 重启（从磁盘重新加载）后再建一个。
        let svc2 = mk_service_rooted_at(&dir, false);
        assert_eq!(svc2.list_socks_nodes().len(), 2, "剩下两个节点应被加载回来");
        let d = svc2.upsert_socks_node(mk(4)).await.unwrap();
        assert!(
            d > c,
            "重启后新节点 id（{d}）必须大于历史上发放过的任何 id（已发过 {c}）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (d) `SocksNodeFileCompat` 的**裸数组**（旧形态）分支：能加载且高水位补齐。
    ///
    /// 该枚举在生产上唯一的引用点是 `load_socks_nodes_from`，测试侧此前为零 ——
    /// 也就是说"旧文件还能不能读"这条兼容承诺从未被验证。
    ///
    /// 回退即 FAIL：删掉 `SocksNodeFileCompat` 的 `BareArray` 变体（只留结构体形态），
    /// 裸数组解析失败 → `load_socks_nodes_from` 走**只读降级**：用户升级后节点表在面板上
    /// 整张消失，且此后任何修改都被拒（"只读降级"），而文件其实是好的。
    #[test]
    fn legacy_bare_array_node_file_loads_and_backfills_next_id() {
        let dir = tmp_dir("compat");
        let path = dir.join("socks_nodes.json");
        // 旧形态：**裸数组**，没有 nextId 这一层。
        std::fs::write(
            &path,
            r#"[{"id":5,"name":"old","url":"socks5://legacy.invalid:1080","enabled":true}]"#,
        )
        .unwrap();

        let svc = mk_service_rooted_at(&dir, false);
        let nodes = svc.list_socks_nodes();
        assert_eq!(nodes.len(), 1, "裸数组旧文件必须能读出来（不得降级成空表）");
        assert_eq!(nodes[0].id, 5);
        assert_eq!(nodes[0].label, "old");
        assert!(nodes[0].enabled, "缺 enabled 字段时应默认 true");

        // 高水位按 max(id)+1 补齐 → 新节点 id 必须 > 5（而不是又发 1）。
        let new_id = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                svc.upsert_socks_node(SocksNodeUpsertRequest {
                    id: None,
                    name: Some("fresh".into()),
                    url: "socks5://fresh.invalid:1080".into(),
                    username: None,
                    password: None,
                    enabled: None,
                })
                .await
            })
            .expect("旧文件之上新建节点");
        assert!(
            new_id > 5,
            "裸数组归一化后 next_id 应至少是 max(id)+1，实得 {new_id}"
        );

        // 回写后应升级成新形态（带 nextId），且旧节点仍在。
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"nextId\""),
            "回写应升级为带高水位的新形态: {raw}"
        );
        let svc2 = mk_service_rooted_at(&dir, false);
        assert_eq!(svc2.list_socks_nodes().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 明文 ↔ 密文开关：同一份数据先明文落盘，改开关后回写即变密文（反之亦然）。
    ///
    /// 这是"透明迁移"承诺的两个方向。回退即 FAIL：`load_socks_nodes_from` 里若去掉
    /// `maybe_decrypt_to_string` 而直接当明文 parse，第二段（密文 → 加载）会解析失败
    /// 进只读降级 —— 开了加密的用户重启后节点表整张消失且无法修改。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_rest_toggle_migrates_both_directions() {
        let dir = tmp_dir("toggle");
        // 先明文写一份。
        let svc_plain = mk_service_rooted_at(&dir, false);
        svc_plain
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("mig".into()),
                url: "socks5://node.invalid:40002".into(),
                username: None,
                password: Some("pw-1".into()),
                enabled: None,
            })
            .await
            .unwrap();
        let path = dir.join("socks_nodes.json");
        assert!(!crate::common::secret_store::is_encrypted(
            &std::fs::read(&path).unwrap()
        ));

        // 打开加密后重启：明文照旧能读（透明迁移），下一次回写才变密文。
        let svc_enc = mk_service_rooted_at(&dir, true);
        assert_eq!(
            svc_enc.list_socks_nodes().len(),
            1,
            "明文文件在开了加密后仍必须能读"
        );
        svc_enc
            .upsert_socks_node(SocksNodeUpsertRequest {
                id: None,
                name: Some("mig2".into()),
                url: "socks5://node2.invalid:40002".into(),
                username: None,
                password: Some("pw-2".into()),
                enabled: None,
            })
            .await
            .unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(
            crate::common::secret_store::is_encrypted(&raw),
            "开了加密后的第一次回写应产出密文"
        );
        for needle in [b"pw-1".as_slice(), b"pw-2".as_slice()] {
            assert!(
                !raw.windows(needle.len()).any(|w| w == needle),
                "迁移后旧密码也不得残留明文"
            );
        }

        // 再关掉加密：密文仍能读（走解密），回写后落回明文。
        let svc_back = mk_service_rooted_at(&dir, false);
        assert_eq!(
            svc_back.list_socks_nodes().len(),
            2,
            "密文在关了加密后仍必须能读"
        );
        svc_back.delete_socks_node(1).ok();
        let raw2 = std::fs::read(&path).unwrap();
        assert!(
            !crate::common::secret_store::is_encrypted(&raw2),
            "关掉加密后的回写应落回明文"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 源码级守卫：组内序号必须在**任何 await 之前**一次性预留完，
    /// 且 `add_credential` 里不得再出现「扫 max 现算」。
    ///
    /// 用源码断言而非行为测试：`add_credential` 会调 `get_usage_limits_for`
    /// （真实上游往返），穿它的行为测试写不了 —— 与本文件既有的
    /// `clone_creation_must_consume_the_node_pool_without_reuse` 同款理由。
    /// 并发正确性本身由 `token_manager` 的
    /// `concurrent_clone_seq_reservations_never_overlap` 覆盖。
    ///
    /// 回退即 FAIL：把序号来源改回 `max_clone_seq_in_group`（无论放在入池前还是入池后）
    /// —— 第 1 条断言立刻失败。两个并发的「给同一 key 加 N 份」请求会各自读到同一个
    /// max，同一组里出现两个 `分身 #2`，管理页无法区分、删除时无法指名。
    #[test]
    fn clone_seq_must_be_reserved_before_any_await() {
        let src = include_str!("service.rs");
        // ⚠️ 两个 needle 都**运行时拼接**：写成完整字面量时它会出现在 include_str!
        // 读到的本测试自身里 —— 否定断言恒失败、肯定断言恒成立（即测试被静默作废）。
        // 本文件已两次踩到这个坑，见节点池那条守卫的注释。
        let scan = format!("{}{}", "max_clone_seq_in", "_group(");
        assert!(
            !src.contains(scan.as_str()),
            "add_credential 不得自行扫 max 现算组内序号：发号与入池之间横跨 await，\
             两个并发请求会读到同一个 max 而重号。序号必须走 token_manager 的原子预留。"
        );
        let reserve = format!("{}{}", "reserve_clone", "_seqs(g, copies)");
        assert!(
            src.contains(reserve.as_str()),
            "必须一次性预留本次全部份数的号段（copies 份），否则第 2..N 份仍会与并发请求撞号"
        );

        // ⭐ 承重的**顺序**断言：预留必须在第一个入池 await 之前。
        // 预留放到入池之后就等于把竞态窗口原样留着（旧代码正是那样）。
        let reserve_at = src.find(reserve.as_str()).expect("上一条断言已保证存在");
        let first_await = format!(
            "{}{}",
            "add_credential_allowing_", "duplicate(new_cred.clone())"
        );
        let await_at = src
            .find(first_await.as_str())
            .expect("第 1 份入池调用应存在");
        assert!(
            reserve_at < await_at,
            "号段预留（位置 {reserve_at}）必须早于第 1 份入池 await（位置 {await_at}）：\
             放在 await 之后等于竞态窗口原封不动"
        );
    }

    /// ⭐ 源码级守卫：`clone_credential` **不得重新实现份数逻辑**，必须复用共享实现。
    ///
    /// 用源码断言而非行为测试：这条路同样会调 `get_usage_limits_for`（真实上游往返）。
    ///
    /// 回退即 FAIL：在 `clone_credential` 里自己抄一遍 copies 循环（哪怕只抄
    /// `add_credential_allowing_duplicate` 那一句）—— 第 2 条断言失败。那会造出第二条
    /// 校验路径：去重绕过、组复用、**序号原子预留**、节点分配、OAuth 拒绝五件事
    /// 各有两份实现，其中任一份漏改就是一个只在某条入口上出现的缺陷。
    #[test]
    fn clone_endpoint_must_reuse_the_shared_copies_path() {
        // ⚠️ 本守卫读源码，必须做**两步归一**，否则它是纸面测试（CLAUDE.md 记载的必备两步）：
        //   ① 剔掉 `//` 开头的行 —— 否则匹配到被注释掉的实现或文档注释里的符号名，
        //      实现被删了守卫仍绿；
        //   ② **去掉全部空白** —— 否则 rustfmt 一次换行就让 needle 失配。
        //
        // 🔴 第 ② 步是 2026-08-06 实测补上的，代价是一次真实红灯：有人给
        // `AddCredentialRequest` 加了字段使调用行变长，rustfmt 于是把
        //     let mut created = self.add_credential_with_intent(
        // 折成
        //     let mut created = self
        //         .add_credential_with_intent(
        // 于是含 `self.` 的 needle 计数从 2 掉到 1、守卫报红，而**代码完全正确**。
        // 当时最省事的"修法"是把断言里的 2 改成 1 —— 那会把守卫彻底作废（它防的是
        // 去重绕过/组复用/序号原子预留/节点分配/OAuth 拒绝五件事各有两份实现）。
        // 归一化之后断言与排版无关，这类假红灯不会再来。
        let raw = include_str!("service.rs");
        let src = normalize_src_for_guard(raw);

        // needle 全部运行时拼接（见节点池守卫处的说明：字面量会匹配到本测试自身，
        // 从而把断言静默作废 —— 本文件已三次踩到这个坑）。
        // ⚠️ 这里的 count 断言尤其要小心：needle 若在本测试源码里出现，它会把自己算进
        // 计数，于是"两处调用都被删掉"仍然满足 `>= 2`。
        // 归一化后本测试自身的拼接式 `format!("{}{}", "self.add_credential_with", ...)`
        // 仍是分开的两段字符串字面量，**不会**自匹配 —— 这是拼接写法在归一化下依然承重的原因。
        let shared = format!("{}{}", "self.add_credential_with", "_intent(");
        assert_eq!(
            src.matches(shared.as_str()).count(),
            2,
            "add_credential 与 clone_credential 必须**都且只**走同一个共享实现\
             （断言已对空白归一，报红说明真的少了一处调用，不是排版问题）"
        );

        // ⭐ 承重：clone_credential 的函数体里不得出现入池调用。
        // ⚠️ needle 按**无空白形状**写（`pub async fn` → `pubasyncfn`），因为 src 已归一化。
        // 原来的带空格写法在归一化后恒不命中 ⇒ `expect` 直接 panic，守卫变成"总是报错"。
        let body_start = src
            .find(format!("{}{}", "pubasyncfnclone_", "credential(").as_str())
            .expect("clone_credential 应存在");
        let body_end = src[body_start..]
            .find(format!("{}{}", "asyncfnadd_credential_with_", "intent(").as_str())
            .map(|off| body_start + off)
            .expect("clone_credential 之后应紧跟共享实现");
        let body = &src[body_start..body_end];
        let insert = format!("{}{}", "add_credential_allowing_", "duplicate");
        assert!(
            !body.contains(insert.as_str()),
            "clone_credential 不得自己入池：份数/去重/序号/节点分配必须只有一份实现"
        );
        let reserve = format!("{}{}", "reserve_clone", "_seqs");
        assert!(
            !body.contains(reserve.as_str()),
            "clone_credential 不得自己预留序号（那会与共享实现各发一段号，重号回归）"
        );

        // 显式意图必须传 true，否则 `copies == 1` 会走去重 → 对已在池中的 key 必然
        // 撞 `凭据已存在`，而「再加 1 份」正是本端点最常见的用法。
        //
        // 🔴 本断言此前的 needle 是 `"            true,\n" + "        )\n        .await"`
        // —— 把**缩进宽度与换行位置**都写进了判据。实测它已经失配到 0 命中（rustfmt
        // 把这个调用收成了一行），只是 `assert_eq!` 在它之前先 panic，所以这条**一直没被
        // 执行过**，没人发现它坏了。这正是"守卫自己烂掉而无人知"的形态：
        // 它比没有守卫更糟，因为它让人以为这件事被钉住了。
        // 归一化后按「无空白形状」写：`...},true).await`。
        let forced = format!("{}{}", "..req},", "true).await");
        assert!(
            src.contains(forced.as_str()),
            "clone_credential 必须以 force_multi_open=true 调共享实现\
             （否则 copies==1 会走去重，对已在池中的 key 必然撞『凭据已存在』）"
        );
    }

    /// 源码级守卫专用的归一化：**剔注释行 + 去全部空白**。
    ///
    /// 这两步是 `CLAUDE.md` 记载的「写源码守卫的必备两步」，缺任一步守卫就是纸面测试：
    ///
    /// - **不剔注释** ⇒ `include_str!` 读到的是含注释的原始文本，把实现整段注释掉后
    ///   `contains` 仍匹配到注释里那行 ⇒ 实现没了守卫还绿。本文件已三次踩到。
    /// - **不去空白** ⇒ rustfmt 把一句调用折成多行就让 needle 失配 ⇒ 代码完全正确却报红。
    ///   2026-08-06 实测发生过：加了个字段使行变长 → rustfmt 换行 → 守卫假红，
    ///   而当时最省事的"修法"是改断言期望值，那等于把守卫作废。
    ///
    /// 去空白而非「归一成单空格」是刻意的：单空格仍然区分 `self .foo(` 与 `self.foo(`，
    /// 而这两者语义完全相同、只差 rustfmt 的一次决定。全去掉才真正与排版无关。
    ///
    /// ⚠️ 代价：needle 也必须写成无空白形状。跨 token 的 needle（如 `fn foo (`）会失配，
    /// 写 needle 时按「删掉所有空格后的样子」写。
    fn normalize_src_for_guard(raw: &str) -> String {
        raw.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .split_whitespace()
            .collect::<String>()
    }

    /// ⭐ 源码级守卫：新端点必须**真的注册进路由**。
    ///
    /// 回退即 FAIL：删掉 `router.rs` 里那行 `.route(...clone...)` —— service 层代码
    /// 还在、编译还过、测试还绿，但前端拿到 404。本仓已有多个"实现了却没挂路由"
    /// 的同类风险点，故把注册这件事钉死。
    #[test]
    fn clone_endpoint_is_registered_in_router() {
        let router = include_str!("router.rs");
        let path = format!("{}{}", "/credentials/{id}", "/clone");
        assert!(
            router.contains(path.as_str()),
            "clone 端点必须注册在 admin 路由树上"
        );
        let handler = format!("{}{}", "post(clone_", "credential)");
        assert!(
            router.contains(handler.as_str()),
            "clone 路由必须绑到 clone_credential 处理器（且是 POST）"
        );
    }

    /// 不存在的 id 必须 404，且**不得**建出任何凭据。
    ///
    /// 这条是 `clone_credential` 唯一不打网络就能穿到底的分支（NotFound 在
    /// `export_credential` 之后立即返回），故可以写真行为测试。
    #[tokio::test]
    async fn cloning_unknown_credential_is_not_found() {
        let svc = mk_service_with_one_credential();
        let before = svc.token_manager.total_count();
        let err = svc
            .clone_credential(9999, 2, None, None, None, None, None)
            .await
            .expect_err("不存在的 id 应报错");
        assert!(
            matches!(err, AdminServiceError::NotFound { id: 9999 }),
            "应是 NotFound，实际 {err:?}"
        );
        assert_eq!(svc.token_manager.total_count(), before, "不得建出任何凭据");
    }

    /// OAuth 号加分身必须被拒，且报错要点名是哪个 id。
    ///
    /// 回退即 FAIL：删掉 `clone_credential` 里那道 `multi_open_rejection_reason` ——
    /// 请求会继续走下去并真的建出 N 份带同一个 refreshToken 的分身，
    /// 它们随后被 `invalid_grant` 逐个自动禁用（面板上显示成"号被封了"）。
    #[tokio::test]
    async fn cloning_oauth_credential_is_rejected_with_id() {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("social".to_string());
        c.refresh_token = Some("rt-oauth".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                None,
                None,
                false,
            )
            .expect("token manager"),
        );
        let svc = AdminService::new(tm, Vec::<String>::new());

        let err = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect_err("OAuth 号不该能加分身");
        let msg = match err {
            AdminServiceError::InvalidCredential(m) => m,
            other => panic!("应是 InvalidCredential，实际 {other:?}"),
        };
        assert!(msg.contains("#1"), "报错应点名 id，实际: {msg}");
        assert!(
            msg.contains("refreshToken"),
            "报错应说明 refreshToken 轮换这个根因，实际: {msg}"
        );
        assert_eq!(svc.token_manager.total_count(), 1, "被拒时不得建出任何份");
    }

    // ============ 分身默认不启用（clone_credential 的 enabled 语义）============
    //
    // 这三条是**真行为**测试，不是源码守卫：断言的是「分身入池后在面板上是 disabled」，
    // 也就是 `get_all_credentials()`（`/credentials/status` 的实现）看到的那个字段。
    //
    // 之所以能穿到底而不打真实上游：`mk_clone_service` 给 token manager 配了一个
    // **必然连不上的本地代理**（`127.0.0.1:1`），于是共享实现里那一次
    // `get_usage_limits_for` 立刻拿 connection refused 并被 `tracing::warn!` 吞掉
    // （它本就是"失败不影响上号"的路径）。同时父号预置了 `region`，共享实现按 key 继承给
    // 分身，于是 `probe_and_persist_api_region` 在廉价预判处就 return —— 全程零 DNS。

    /// 造一个「加分身能穿到底且不出网」的 service：父号是 api_key + 预置 region，
    /// 全局代理指向必然拒连的 127.0.0.1:1。
    fn mk_clone_service() -> AdminService {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_clone_enabled_test".to_string());
        // 预置 region → 共享实现继承给每份 → region 探测在预判处返回，不出网。
        c.region = Some("us-east-1".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![c],
                // 拒连代理：让唯一那次上游往返立刻失败，测试与网络环境无关。
                Some(crate::http_client::ProxyConfig::new("http://127.0.0.1:1")),
                None,
                false,
            )
            .expect("token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 面板视角下「除父号 #1 之外的每一份」的 disabled 状态。
    fn clone_disabled_flags(svc: &AdminService) -> Vec<(u64, bool)> {
        let mut v: Vec<(u64, bool)> = svc
            .get_all_credentials()
            .credentials
            .into_iter()
            .filter(|c| c.id != 1)
            .map(|c| (c.id, c.disabled))
            .collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// ⭐ `enabled` 省略 → **每一份**分身入池即禁用，父号状态不变。
    ///
    /// 回退即 FAIL：把 `clone_credential` 里那句 `disabled: !enabled.unwrap_or(false)`
    /// 改回旧行为（删掉该字段 / 写 `disabled: false`）—— 本条的 `all disabled` 断言变红。
    ///
    /// 为什么必须是"入池时就 disabled"而不是"建完再批量禁用"：后者有中间窗口，
    /// 分身在那段时间里是启用的，调度器立刻往它们发流量。实测事故
    /// （2026-08-05 02:42）一次 copies=5，4 个分身 region 错配 → 恒 403 →
    /// **24 秒内全部被自动禁用、0% 成功**，那 24 秒的真实用户请求全打在必废的号上。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloned_credentials_are_disabled_by_default() {
        let svc = mk_clone_service();
        let resp = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect("加分身应成功");

        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 3, "copies=3 应建出 3 份，实际 {ids:?}");
        assert_eq!(svc.token_manager.total_count(), 4, "父号 + 3 份分身");

        let flags = clone_disabled_flags(&svc);
        assert_eq!(flags.len(), 3, "父号之外应恰好 3 份，实际 {flags:?}");
        assert!(
            flags.iter().all(|(_, disabled)| *disabled),
            "省略 enabled 时每一份分身都必须是禁用态，实际 {flags:?}"
        );

        // 父号本身绝不能被顺手改状态。
        let parent = svc
            .get_all_credentials()
            .credentials
            .into_iter()
            .find(|c| c.id == 1)
            .expect("父号必须还在");
        assert!(!parent.disabled, "父号的启用状态不该被加分身影响");

        // available 只数未禁用的 → 仍然只有父号一个可用。
        assert_eq!(
            svc.get_all_credentials().available,
            1,
            "禁用的分身不得计入可用数（否则面板容量与调度池对不上）"
        );
    }

    /// `enabled: true` → 分身建出来就是启用的（这个开关必须真的双向可控）。
    ///
    /// 回退即 FAIL：把那句改成硬编码 `disabled: true` —— 本条变红。
    /// 有这一条，上一条才不可能靠"永远禁用"蒙过去。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloned_credentials_can_be_created_enabled_on_request() {
        let svc = mk_clone_service();
        svc.clone_credential(1, 2, Some(true), None, None, None, None)
            .await
            .expect("加分身应成功");

        let flags = clone_disabled_flags(&svc);
        assert_eq!(flags.len(), 2, "copies=2 应建出 2 份，实际 {flags:?}");
        assert!(
            flags.iter().all(|(_, disabled)| !*disabled),
            "显式 enabled=true 时分身必须是启用态，实际 {flags:?}"
        );
        assert_eq!(svc.get_all_credentials().available, 3, "父号 + 2 份都可用");
    }

    /// `enabled: false` 显式给出时与省略同义（前端可能两种都发）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_enabled_false_matches_the_omitted_default() {
        let svc = mk_clone_service();
        svc.clone_credential(1, 2, Some(false), None, None, None, None)
            .await
            .expect("加分身应成功");
        let flags = clone_disabled_flags(&svc);
        assert_eq!(flags.len(), 2);
        assert!(
            flags.iter().all(|(_, disabled)| *disabled),
            "显式 false 必须与省略同义，实际 {flags:?}"
        );
    }

    /// `enabled` 的 JSON 契约：省略 → `None`（由 service 落到"禁用"），
    /// 显式 `true` / `false` 各自原样解出。
    ///
    /// 回退即 FAIL：给该字段加上 `#[serde(default = "...")]` 之类把 None 提前吃掉的
    /// 默认值 —— 第一条断言变红（service 层就再也分不清"没给"与"给了 false"）。
    #[test]
    fn clone_request_parses_enabled_as_optional_camel_case() {
        use crate::admin::types::CloneCredentialRequest;

        let omitted: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":3}"#).expect("省略 enabled 应能解析");
        assert_eq!(omitted.copies, Some(3));
        assert_eq!(omitted.enabled, None, "省略时必须是 None，不能被吃成 false");

        let on: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":2,"enabled":true}"#).expect("解析 enabled=true");
        assert_eq!(on.enabled, Some(true));

        let off: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":2,"enabled":false}"#).expect("解析 enabled=false");
        assert_eq!(off.enabled, Some(false));
    }

    // ============ 节点池 → 各份的分配（含主份）============
    //
    // 这一组是**真行为**测试而不是源码守卫：断言的是「入池后每一份的 proxyUrl 到底是什么」，
    // 也就是 `export_credential` 看到的那个字段。能穿到底不出网的理由与上面 `enabled`
    // 那三条相同（`mk_clone_service` 的拒连代理 + 预置 region）。
    //
    // ⚠️ 节点 URL 一律用 RFC 6761 保留的 `.invalid` TLD（与既有节点测试同款）：
    // `upsert_socks_node` 会对节点 URL 做 SSRF 校验，`127.0.0.1` 会被**正确地**拒绝
    // （`目标解析到非公网地址 127.0.0.1`），所以环回地址在这条路上根本进不了池。
    // `.invalid` 保证永不解析 → 走 DNS 失败的 fail-open 分支入池，而随后那一次
    // `get_usage_limits_for` 也在 DNS 处即失败，测试与本机 DNS/代理环境无关
    // （见 CLAUDE.md 已知问题 #19 的同款理由）。

    /// 节点 i（0-based）的 URL。逐个不同，断言才能区分是哪个节点。
    fn node_url(i: usize) -> String {
        format!("socks5://node{}.invalid:{}", i + 1, 40001 + i)
    }

    /// 往池里塞 n 个启用节点，返回它们的 id（顺序 = 插入顺序）。
    async fn seed_nodes(svc: &AdminService, n: usize) -> Vec<u64> {
        let mut ids = Vec::new();
        for i in 0..n {
            let id = svc
                .upsert_socks_node(SocksNodeUpsertRequest {
                    id: None,
                    name: Some(format!("n{i}")),
                    url: node_url(i),
                    username: None,
                    password: None,
                    enabled: Some(true),
                })
                .await
                .expect("加节点应成功");
            ids.push(id);
        }
        ids
    }

    /// 逐 id 取「这一份的 proxyUrl」。
    ///
    /// 走 `token_manager.export_credential`（原始值）而不是 `AdminService::export_credential`
    /// —— 后者是给导出用的、会做脱敏，断言出口 URL 必须看原始值。
    fn proxy_urls_by_id(svc: &AdminService, ids: &[u64]) -> Vec<Option<String>> {
        ids.iter()
            .map(|id| {
                svc.token_manager
                    .export_credential(*id)
                    .unwrap_or_else(|| panic!("凭据 #{id} 应存在"))
                    .proxy_url
            })
            .collect()
    }

    /// 🔴 承重（缺陷 A）：**主份也要拿节点**，只要它自己没有代理。
    ///
    /// 实测的旧行为：池里 5 个全启用、一次 `copies=4`，只有第 2/3/4 份拿到节点，
    /// **主份裸连**，两个节点闲置 —— 而用户以为 4 份都分散了。
    ///
    /// 回退即 FAILED：把节点计划挪回 `copies > 1` 块内（即第 1 份入池之后再算），
    /// 或把 `pool_may_assign` 的判据改回「是不是第 1 份」—— 第一条断言变红
    /// （`urls[0]` 是 None）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_copy_must_get_a_node_when_it_has_no_proxy_of_its_own() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 3, "copies=3 应建出 3 份，实际 {ids:?}");

        let urls = proxy_urls_by_id(&svc, &ids);
        // ⭐ 这一条是整个缺陷 A：修复前它恒为 None。
        assert!(
            urls[0].is_some(),
            "主份必须也从节点池拿到出口（它是全新条目、本来没代理），实际 {urls:?}"
        );
        // 三份三节点 → 每份都有，且**互不相同**（不复用）。
        assert!(
            urls.iter().all(|u| u.is_some()),
            "3 个启用节点 / 3 份应全部分到，实际 {urls:?}"
        );
        let mut distinct: Vec<&str> = urls.iter().map(|u| u.as_deref().unwrap()).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            3,
            "各份出口必须互不相同（复用等于没分散），实际 {urls:?}"
        );

        // 文案要如实：3 份全额分配 → 不得出现"直连"字样。
        assert!(
            resp.message.contains("已从节点池为 3 份分配独立出口 IP"),
            "文案应如实报 3 份，实际: {}",
            resp.message
        );
        assert!(
            !resp.message.contains("直连"),
            "全额分配时不得声称有份直连，实际: {}",
            resp.message
        );
    }

    /// 🔴 承重（缺陷 A 的另一半 / 零回归）：主份**已有代理**时绝不覆盖。
    ///
    /// 这是原注释真正要保护的东西（"覆盖会把一个在跑的号的出口换掉"），
    /// 修复后必须仍然成立。走 `add_credential_with_intent` 而不是 `clone_credential`：
    /// 后者刻意把 proxy_* 留空，构造不出"调用方已显式指定代理"这个场景。
    ///
    /// 回退即 FAILED：把 `pool_may_assign` 恒设为 true（即不再看这一份有没有代理）
    /// —— 第一条断言变红（主份的出口被池节点顶掉）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_proxy_must_never_be_overwritten_by_the_pool() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = svc
            .add_credential_with_intent(
                AddCredentialRequest {
                    auth_method: "api_key".into(),
                    kiro_api_key: Some("ksk_clone_enabled_test".into()),
                    copies: Some(2),
                    // 调用方的明确意图：这一批就要走这个出口。
                    proxy_url: Some("socks5://127.0.0.1:9".into()),
                    disabled: true,
                    ..Default::default()
                },
                false,
            )
            .await
            .expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert!(
            urls.iter()
                .all(|u| u.as_deref() == Some("socks5://127.0.0.1:9")),
            "显式给了 proxy_url 时池分配必须完全不介入（每份都保持调用方给的那个），实际 {urls:?}"
        );
        assert!(
            resp.message.contains("未从节点池分配代理"),
            "文案应说明本次没走池分配，实际: {}",
            resp.message
        );
    }

    /// ⭐ 承重（缺陷 B）：`nodeIds` 给了就**按顺序**分给各份，池里其余节点一律不用。
    ///
    /// 回退即 FAILED：让 `resolve_node_plan` 忽略 `node_ids`（恒走"池里全部启用节点"
    /// 那一支）—— 各份会拿到 #1/#2/#3 也就是端口 1/2/3，而本条要求的是端口 3/1
    /// （用户挑的那两个，且顺序是他给的顺序）→ 断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_node_ids_are_assigned_in_the_given_order() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 3).await;

        let resp = svc
            // 刻意**倒序**且只挑两个：既验证"按给定顺序"，也验证"没挑的节点不会被顶上来"。
            .clone_credential(1, 2, None, Some(vec![nodes[2], nodes[0]]), None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert_eq!(
            urls,
            vec![Some(node_url(2)), Some(node_url(0))],
            "必须严格按 nodeIds 的顺序分（第 1 个给主份），实际 {urls:?}"
        );
        // 没被挑中的第 2 个节点绝不能出现。
        assert!(
            !urls
                .iter()
                .any(|u| u.as_deref() == Some(node_url(1).as_str())),
            "未被指定的节点不得被用上，实际 {urls:?}"
        );
        assert!(
            resp.message.contains("已从节点池为 2 份分配独立出口 IP"),
            "文案应如实报 2 份，实际: {}",
            resp.message
        );
    }

    /// ⭐ 承重（缺陷 B + C）：不存在 / 已禁用的 node id **跳过并点名**，绝不静默替换。
    ///
    /// 这是需求 C 的核心：「我选了节点却仍然直连」是最容易踩空的一步，
    /// 而**静默换一个节点**更糟 —— 用户以为出口是他挑的那个。
    ///
    /// 回退即 FAILED：
    /// - 让 `resolve_node_plan` 把无效 id 静默替换成池里下一个可用节点 →
    ///   第 2 份会拿到端口 2 而不是直连 → 第二条断言变红；
    /// - 或者把 `rejected` 从文案里删掉 → 后两条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_node_ids_are_skipped_and_named_in_the_message() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 2).await;
        // 把第 2 个关掉：显式指定也不该用它（否则「禁用」这个开关在这条路上形同不存在）。
        svc.upsert_socks_node(SocksNodeUpsertRequest {
            id: Some(nodes[1]),
            name: None,
            url: node_url(1),
            username: None,
            password: None,
            enabled: Some(false),
        })
        .await
        .expect("禁用节点应成功");

        let missing = 9999u64;
        let resp = svc
            .clone_credential(
                1,
                2,
                None,
                Some(vec![nodes[0], nodes[1], missing]),
                None,
                None,
                None,
            )
            .await
            .expect("加分身应成功（无效 id 不该让整个请求失败）");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert_eq!(
            urls[0].as_deref(),
            Some(node_url(0).as_str()),
            "有效的那个必须生效，实际 {urls:?}"
        );
        // ⭐ 承重：第 2 份**直连**，而不是被悄悄塞上别的节点。
        assert!(
            urls[1].is_none(),
            "无效 id 必须跳过、该份直连；静默替换会让用户以为出口是他选的那个。实际 {urls:?}"
        );

        // ⭐ 需求 C：两个无效 id 都要在文案里点名，且写清各自原因。
        let msg = &resp.message;
        assert!(
            msg.contains(&format!("#{}（已禁用）", nodes[1])),
            "被禁用的节点必须点名且注明原因，实际: {msg}"
        );
        assert!(
            msg.contains(&format!("#{missing}（不存在）")),
            "不存在的节点必须点名且注明原因，实际: {msg}"
        );
        assert!(
            msg.contains("已从节点池为 1 份分配独立出口 IP")
                && msg.contains("另有 1 份因启用节点不足而直连"),
            "文案必须同时报「分了几份」与「几份直连」，实际: {msg}"
        );
    }

    /// 重复的 node id 记作 `重复` 并只用一次（两份共用一个出口就是"复用"，
    /// 而复用等于没分散 —— 调用方显式写两遍也不例外，只是这次要说出来）。
    ///
    /// 回退即 FAILED：去掉 `resolve_node_plan` 里的查重 —— 两份都拿到端口 1，
    /// 第二条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_node_ids_are_used_once_and_reported() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 2).await;

        let resp = svc
            .clone_credential(1, 2, None, Some(vec![nodes[0], nodes[0]]), None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert_eq!(
            urls[0].as_deref(),
            Some(node_url(0).as_str()),
            "第一次出现的应生效，实际 {urls:?}"
        );
        assert!(
            urls[1].is_none(),
            "同一个节点不得被两份共用（那等于没分散），实际 {urls:?}"
        );
        assert!(
            resp.message.contains(&format!("#{}（重复）", nodes[0])),
            "重复的 id 必须点名，实际: {}",
            resp.message
        );
    }

    /// 启用节点少于份数时：够的份分到，其余**直连**（刻意不轮询复用），文案如实。
    ///
    /// 回退即 FAILED：把取用改成取模复用（`% assignable.len()`）—— 第二条
    /// "互不相同"的断言变红。这条同时是那道源码守卫
    /// `clone_creation_must_consume_the_node_pool_without_reuse` 的行为侧对照。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fewer_nodes_than_copies_leaves_the_rest_direct_without_reuse() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;

        let resp = svc
            .clone_credential(1, 4, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 4, "copies=4 应建出 4 份，实际 {ids:?}");
        let urls = proxy_urls_by_id(&svc, &ids);

        let with_proxy: Vec<&str> = urls.iter().filter_map(|u| u.as_deref()).collect();
        assert_eq!(
            with_proxy.len(),
            2,
            "只有 2 个节点 → 只能有 2 份带出口，实际 {urls:?}"
        );
        let mut d = with_proxy.clone();
        d.sort_unstable();
        d.dedup();
        assert_eq!(
            d.len(),
            2,
            "带出口的两份必须用不同节点（不复用），实际 {urls:?}"
        );
        // 前两份拿到、后两份直连（顺序是承重的：份序与节点序一一对应）。
        assert!(
            urls[0].is_some() && urls[1].is_some() && urls[2].is_none() && urls[3].is_none(),
            "应按份序分配、不够的份直连，实际 {urls:?}"
        );
        assert!(
            resp.message.contains("已从节点池为 2 份分配独立出口 IP")
                && resp.message.contains("另有 2 份因启用节点不足而直连"),
            "文案必须如实报 2 分配 / 2 直连，实际: {}",
            resp.message
        );
    }

    // ============ 主份开关 / 自动分配排序 / 节点不足（4.1 · 4.3 · 4.4）============
    //
    // 全部穿 `add_credential_with_intent` 或 `clone_credential` 这两条**真实入口**，
    // 断言的是「入池后每一份的 proxyUrl 到底是什么」。
    // 刻意不直接测 `resolve_node_plan`：它是私有纯函数，而真实链路上排在它之前的
    // `pool_may_assign` / `primary_pinned_node` / `is_multi_open` 三道门都能把它的结果
    // 全部作废 —— 只测纯函数就是「测了分支内部，没测分支之间」那一类无效修复。

    /// 走普通上号入口（`POST /credentials` 等价路径）建 N 份。
    async fn add_copies(
        svc: &AdminService,
        copies: u32,
        mutate: impl FnOnce(&mut AddCredentialRequest),
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        let mut req = AddCredentialRequest {
            auth_method: "api_key".into(),
            kiro_api_key: Some("ksk_clone_enabled_test".into()),
            copies: Some(copies),
            disabled: true,
            ..Default::default()
        };
        mutate(&mut req);
        svc.add_credential_with_intent(req, false).await
    }

    /// 🔴 承重（4.1，开关**关**=缺省）：`POST /credentials` + `copies=3` 时
    /// **主份不从池取节点**，三个节点里只有 2 个被第 2/3 份消费。
    ///
    /// 回退即 FAILED：把 `assign_primary` 改回恒 true（即删掉
    /// `req.assign_primary_node.unwrap_or(copies == 1)` 这道门）—— 主份会拿到节点，
    /// 第一条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn primary_does_not_take_a_pool_node_by_default_on_the_add_path() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = add_copies(&svc, 3, |_| {}).await.expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert!(
            urls[0].is_none(),
            "开关缺省（关）时主份必须保持自身出口（这里是无代理），实际 {urls:?}"
        );
        assert!(
            urls[1].is_some() && urls[2].is_some(),
            "第 2/3 份必须各拿到一个节点，实际 {urls:?}"
        );
        assert_ne!(urls[1], urls[2], "两份不得共用一个出口，实际 {urls:?}");
        // ⭐ 文案不得把「按设置刻意直连的主份」算进"因启用节点不足而直连"。
        assert!(
            resp.message.contains("已从节点池为 2 份分配独立出口 IP"),
            "应如实报 2 份，实际: {}",
            resp.message
        );
        assert!(
            !resp.message.contains("因启用节点不足而直连"),
            "主份是按设置直连，不是节点不够——这句是假归因。实际: {}",
            resp.message
        );
        assert!(
            resp.message.contains("主份按「主份也从池取节点=关」"),
            "必须说明主份为何没有出口，实际: {}",
            resp.message
        );
    }

    /// 🔴 承重（4.1，开关**开**）：显式 `assignPrimaryNode=true` 时主份也拿节点，
    /// 三份三节点全额分配。
    ///
    /// 回退即 FAILED：让 `assign_primary` 恒 false —— 主份不再拿节点，第一条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn primary_takes_a_pool_node_when_the_switch_is_on() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 3).await;

        let resp = add_copies(&svc, 3, |r| r.assign_primary_node = Some(true))
            .await
            .expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert!(urls[0].is_some(), "开关开时主份必须拿到节点，实际 {urls:?}");
        let mut d: Vec<&str> = urls.iter().map(|u| u.as_deref().unwrap()).collect();
        d.sort_unstable();
        d.dedup();
        assert_eq!(d.len(), 3, "三份必须各自不同出口，实际 {urls:?}");
        assert!(
            resp.message.contains("已从节点池为 3 份分配独立出口 IP")
                && !resp.message.contains("主份按"),
            "开关开时不该出现「主份不参与」那句，实际: {}",
            resp.message
        );
    }

    /// 反序列化兼容（4.1 的硬要求）：两个请求体缺字段时都必须能解析成 `None`，
    /// 且 `None` 在各自入口上被解读成**各自的既有行为**。
    ///
    /// 回退即 FAILED：把字段写成非 `Option`（或去掉 `#[serde(default)]`）——
    /// 前两条 `expect` 直接 panic（老前端只发 `{"copies":3}` / 一堆身份字段）。
    #[test]
    fn new_node_switches_are_optional_and_default_to_existing_behavior() {
        use crate::admin::types::CloneCredentialRequest;

        // ① clone 入口：老前端的请求体必须照旧能解析。
        let old_clone: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":3}"#).expect("老 clone 请求体必须能解析");
        assert_eq!(old_clone.assign_primary_node, None);
        assert_eq!(old_clone.require_node_per_copy, None);

        // ② add 入口：老前端的请求体必须照旧能解析。
        let old_add: AddCredentialRequest =
            serde_json::from_str(r#"{"authMethod":"api_key","kiroApiKey":"ksk_x","copies":2}"#)
                .expect("老 add 请求体必须能解析");
        assert_eq!(old_add.assign_primary_node, None);
        assert_eq!(old_add.require_node_per_copy, None);
        assert_eq!(old_add.primary_node_id, None);

        // ③ camelCase 线上格式必须解得出（写成 snake_case 就永远收不到前端的值）。
        let given: AddCredentialRequest = serde_json::from_str(
            r#"{"authMethod":"api_key","assignPrimaryNode":true,"requireNodePerCopy":true,"primaryNodeId":7}"#,
        )
        .expect("camelCase 必须能解析");
        assert_eq!(given.assign_primary_node, Some(true));
        assert_eq!(given.require_node_per_copy, Some(true));
        assert_eq!(given.primary_node_id, Some(7));

        // ④ clone 入口的 `None` 必须被解读成 true —— 这是"升级后行为不变"的那一半：
        //    裸 `#[serde(default)]` 的 false 会让老前端静默退回 2026-08-05 修掉的缺陷
        //    （主份裸连、池里空着一个节点）。这里锁的是 service 层那句 `unwrap_or(true)`。
        let src = include_str!("service.rs");
        let needle = format!(
            "{}{}",
            "assign_primary_node: Some(assign_primary_node.", "unwrap_or(true))"
        );
        assert!(
            src.contains(needle.as_str()),
            "clone_credential 必须把缺省解读成 true，否则老前端退回主份裸连的旧缺陷"
        );
    }

    /// 🔴 承重（4.3）：自动分配按「已绑凭据数」升序、同数按延迟升序。
    ///
    /// 构造（3 个启用节点 + 1 个测活失败的）：
    /// | 节点 | 已绑 | 延迟 | 期望顺序 |
    /// |---|---|---|---|
    /// | n0 | 0 | 300ms | 第 2 |
    /// | n1 | **1**（父号绑着它）| 100ms | 第 3（已绑数是主键，延迟最低也排最后）|
    /// | n2 | 0 | 200ms | **第 1** |
    /// | n3 | 0 | 50ms + `ok=false` | 不参与（已知不通）|
    ///
    /// 一条断言同时钉住三件事：已绑数是主键（n1 最后）、延迟是次键（n2 在 n0 前）、
    /// 测活失败被排除（n3 不出现，尽管它延迟最低）。
    ///
    /// 回退即 FAILED：把排序键改回插入顺序（`sort_by_key` 那行删掉）→ 顺序变 n0/n1/n2；
    /// 或去掉 `last_test` 的 ok 过滤 → n3 会以 50ms 排到第一。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_assignment_orders_by_bound_count_then_latency() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 4).await;

        // 父号 #1 绑上 n1 → n1 的「已绑数」= 1（启发式按 proxy_url 字符串比对）。
        svc.token_manager
            .set_credential_proxy(1, Some(node_url(1)), None, None)
            .expect("给父号绑节点应成功");

        let mk_test = |ok: bool, latency: u64| crate::kiro::model::socks_node::SocksNodeTest {
            ok,
            latency_ms: latency,
            exit_ip: None,
            error: None,
            tested_at: 1,
        };
        svc.record_socks_node_test(nodes[0], mk_test(true, 300))
            .unwrap();
        svc.record_socks_node_test(nodes[1], mk_test(true, 100))
            .unwrap();
        svc.record_socks_node_test(nodes[2], mk_test(true, 200))
            .unwrap();
        // 已知不通：延迟最低但必须被排除。
        svc.record_socks_node_test(nodes[3], mk_test(false, 50))
            .unwrap();

        // clone 路径（主份也参与，缺省 true）建 3 份 → 按序应拿 n2 / n0 / n1。
        let resp = svc
            .clone_credential(1, 3, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert_eq!(
            urls,
            vec![Some(node_url(2)), Some(node_url(0)), Some(node_url(1))],
            "顺序必须是 (已绑数↑, 延迟↑)：n2(0/200) → n0(0/300) → n1(1/100)。实际 {urls:?}"
        );
        assert!(
            !urls
                .iter()
                .any(|u| u.as_deref() == Some(node_url(3).as_str())),
            "最近测活失败的节点不得参与自动分配（它延迟最低，靠这条才能区分排序与过滤），实际 {urls:?}"
        );
    }

    /// 4.3 的另一半：`boundCredentials` 必须真的下发给前端。
    ///
    /// 前端的节点下拉与「自动分配」按钮按它排序，与后端 `resolve_node_plan` 同一口径。
    /// 回退即 FAILED：`list_socks_nodes` 改回 `map(SocksNodeView::from_node)`（恒 0）——
    /// 第二条断言变红，前端排序退化成插入顺序而后端仍按已绑数，两边推荐不一致。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_list_reports_bound_credential_count() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 2).await;
        svc.token_manager
            .set_credential_proxy(1, Some(node_url(1)), None, None)
            .expect("绑节点应成功");

        let listed = svc.list_socks_nodes();
        let by_id = |id: u64| {
            listed
                .iter()
                .find(|v| v.id == id)
                .unwrap_or_else(|| panic!("节点 #{id} 应在列表里"))
                .bound_credentials
        };
        assert_eq!(by_id(nodes[0]), 0, "没号绑它 → 0");
        assert_eq!(by_id(nodes[1]), 1, "父号绑着它 → 1");
    }

    /// 🔴 承重（4.4）：严格模式下节点不足 → **报错且一份也不建**，绝不复用。
    ///
    /// 回退即 FAILED：删掉那段 `require_node_per_copy == Some(true)` 的检查 ——
    /// 请求会成功建出 4 份（2 份带出口、2 份直连），前两条断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn strict_mode_errors_instead_of_creating_copies_without_nodes() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;
        let before = svc.token_manager.total_count();

        let err = add_copies(&svc, 4, |r| {
            r.assign_primary_node = Some(true);
            r.require_node_per_copy = Some(true);
        })
        .await
        .expect_err("节点不足时必须报错，而不是建出一堆共用出口的份");

        let msg = err.to_string();
        assert!(
            msg.contains("节点不足") && msg.contains("需要 4 个") && msg.contains("只有 2 个"),
            "报错必须说清需要几个/实际几个，实际: {msg}"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            before,
            "严格模式失败时**一份都不该建出来**（否则是「建了一半再报错」）"
        );
    }

    /// 4.4 的宽松侧（零回归）：不开严格模式时行为逐字不变 —— 节点不够就直连，不报错。
    ///
    /// 这条是上一条的对照组：没有它，「严格模式」可能被写成"恒严格"而测不出来。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lenient_mode_still_falls_back_to_direct_without_error() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;

        let resp = add_copies(&svc, 4, |r| r.assign_primary_node = Some(true))
            .await
            .expect("缺省（宽松）时节点不够也必须成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        assert_eq!(ids.len(), 4, "宽松模式应照旧建出 4 份，实际 {ids:?}");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert_eq!(
            urls.iter().filter(|u| u.is_some()).count(),
            2,
            "只有 2 个节点 → 只能有 2 份带出口（其余直连，不复用），实际 {urls:?}"
        );
    }

    /// 🔴 承重（4.4 的位置）：严格模式的检查必须排在 `reserve_clone_seqs` **之前**。
    ///
    /// 源码级顺序断言（同款范式见 `clone_seq_must_be_reserved_before_any_await`）：
    /// 放在号段预留之后时，每次"节点不够"的失败都会白烧掉一段组内序号 →
    /// 分身管理页上留下永久空洞（#1 #2 #3 #7 #8），而重试一次就再烧一段。
    /// 这一条测的是**分支之间的顺序**，行为测试测不出来（两种顺序都返回同一个错误）。
    #[test]
    fn strict_node_check_must_run_before_reserving_clone_seqs() {
        let src = include_str!("service.rs");
        // needle 运行时拼接：写成字面量会被 include_str! 读到本测试自身，
        // 于是两个 find 都命中这里、顺序恒成立 —— 断言静默作废。
        let check = format!(
            "{}{}",
            "req.require_node_per_copy == ", "Some(true) && pool_may_assign"
        );
        let reserve = format!(
            "{}{}",
            "self.token_manager.reserve_clone", "_seqs(g, copies)"
        );
        let check_at = src.find(check.as_str()).expect("严格模式检查应存在");
        let reserve_at = src.find(reserve.as_str()).expect("号段预留应存在");
        assert!(
            check_at < reserve_at,
            "节点不足检查（位置 {check_at}）必须早于号段预留（位置 {reserve_at}）：\
             放在之后会让每次失败都白烧一段组内序号，分身页上留永久空洞"
        );
    }

    /// 🔴 承重（4.2 的后端侧）：`primaryNodeId` 点名的节点写进主份，
    /// 且该节点**不会**再被第 2..N 份分到。
    ///
    /// 为什么不复用 `nodeIds[0]`：`nodeIds` 的语义是"本次只用这些"，于是
    /// `copies=3 + nodeIds=[X]` 会让第 2/3 份一个节点都拿不到。本字段只钉主份，
    /// 其余份仍从池里自动补。
    ///
    /// 回退即 FAILED：把 `primary_node_id` 的处理删掉 → 主份变直连（第一条断言红）；
    /// 或不把它从计划里排除（`exclude_id` 那两个 filter）→ 有一份会与主份共用出口
    /// （第三条断言红）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn primary_node_id_pins_the_primary_and_is_excluded_from_the_rest() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 3).await;

        let resp = add_copies(&svc, 3, |r| r.primary_node_id = Some(nodes[1]))
            .await
            .expect("多开应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);

        assert_eq!(
            urls[0].as_deref(),
            Some(node_url(1).as_str()),
            "主份必须走点名的那个节点，实际 {urls:?}"
        );
        assert!(
            urls[1].is_some() && urls[2].is_some(),
            "第 2/3 份仍应从池里自动补（点名主份不该把池锁死），实际 {urls:?}"
        );
        let mut d: Vec<&str> = urls.iter().map(|u| u.as_deref().unwrap()).collect();
        d.sort_unstable();
        d.dedup();
        assert_eq!(
            d.len(),
            3,
            "点名的节点不得被第 2..N 份再分一次，实际 {urls:?}"
        );
    }

    /// `primaryNodeId` 指向不存在 / 已禁用的节点 → **400 且不建任何份**。
    ///
    /// 静默直连或静默换一个节点都会让用户以为出口是他刚点的那个（与 `nodeIds`
    /// 那条"不静默替换"同一原则，只是这里是他唯一的选择，故直接拒绝而不是跳过）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_primary_node_id_is_rejected_without_creating_anything() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 1).await;
        let before = svc.token_manager.total_count();

        let err = add_copies(&svc, 2, |r| r.primary_node_id = Some(9999))
            .await
            .expect_err("不存在的节点 id 必须报错");
        assert!(
            err.to_string().contains("#9999 不存在"),
            "必须点名那个 id 与原因，实际: {err}"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            before,
            "报错时不得建出任何份"
        );
    }

    /// `nodeIds` 的 JSON 契约：省略 → `None`（走自动分配），给了则原样解出。
    ///
    /// 回退即 FAILED：把字段写成非 `Option` 或去掉 `#[serde(default)]` ——
    /// 第一条断言（老前端只发 `{"copies":3}`）直接解析失败。
    #[test]
    fn clone_request_parses_node_ids_as_optional_camel_case() {
        use crate::admin::types::CloneCredentialRequest;

        let omitted: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":3}"#).expect("省略 nodeIds 应能解析");
        assert_eq!(omitted.node_ids, None, "省略时必须是 None（走自动分配）");

        let given: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":4,"enabled":false,"nodeIds":[1,5,6,9]}"#)
                .expect("解析 nodeIds");
        assert_eq!(given.node_ids, Some(vec![1, 5, 6, 9]));
        assert_eq!(given.copies, Some(4));
        assert_eq!(given.enabled, Some(false));

        // 空数组与省略同义（前端可能两种都发）——语义在 service 层收口，
        // 这里只锁「能解析成空 Vec 而不是报错」。
        let empty: CloneCredentialRequest =
            serde_json::from_str(r#"{"copies":2,"nodeIds":[]}"#).expect("解析空 nodeIds");
        assert_eq!(empty.node_ids, Some(vec![]));
    }

    /// ⭐ 节点 id **永不复用**，包括「删掉最大 id 后再新建」。
    ///
    /// 回退即 FAIL：把 id 分配改回 `nodes.iter().map(|n| n.id).max().unwrap_or(0) + 1`
    /// —— 删掉 #2 后新建又得到 #2，而面板另一个标签页仍持有删除前的列表，
    /// 点它的「测活」会打到这个无关的新节点上。
    #[tokio::test]
    async fn node_ids_are_never_reused_after_deleting_the_highest() {
        let svc = mk_service_with_one_credential();
        let mk = |n: u16| SocksNodeUpsertRequest {
            id: None,
            name: Some(format!("n{n}")),
            url: format!("socks5://node{n}.invalid:40002"),
            username: None,
            password: None,
            enabled: None,
        };
        let a = svc.upsert_socks_node(mk(1)).await.unwrap();
        let b = svc.upsert_socks_node(mk(2)).await.unwrap();
        assert!(b > a);

        assert!(svc.delete_socks_node(b).await_ok());
        let c = svc.upsert_socks_node(mk(3)).await.unwrap();
        assert!(
            c > b,
            "删掉最大 id 后新建必须拿到更大的 id（实得 {c}，已发放过 {b}）"
        );
    }

    // ============ 同 key「无独立出口」告警 + 组标识回填 ============
    //
    // 线上实测的形态（本组测试的依据）：`#776` keyHash=029fdd8929、**无 cloneGroup、
    // 无代理**；`#778–787` 同 key 同组、各有独立 SOCKS ⇒ 11 份共用一个上游账号，
    // 其中 1 份走服务器裸 IP。`mk_clone_service` 的父号 `#1` 与 `#776` 完全同构
    // （api_key、无 proxy_url、无 clone_group），所以这组测试就是那个场景本身。

    /// 父号在池中的原始快照（用来断言「除了组标识，一个字段都没被动」）。
    fn parent_snapshot(svc: &AdminService) -> crate::kiro::model::credentials::KiroCredentials {
        svc.token_manager
            .export_credential(1)
            .expect("父号 #1 必须存在")
    }

    /// 🔴 承重（任务一）：同 key 有份**没有独立出口**时必须告警，
    /// 且**绝不**因此改动它的 `proxy_url`。
    ///
    /// 两条断言各自钉一件事，缺任何一条都会漏掉一类回归：
    /// - 告警出现 → 防「静默」（用户在面板上看到 N 份都有 socks，唯独那一份看不出来）
    /// - 父号 `proxy_url` 仍为 `None` → 防「好心自动分配」（用户已明确拍板不要，
    ///   `proxy_url` 是显式配置，直连也可能是刻意留的对照）
    ///
    /// 回退即 FAILED：删掉 `bare_exit_note` 那段 → 第一条变红；
    /// 把它改成「顺手给无出口的号 `set_credential_proxy`」→ 第二条变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloning_warns_about_same_key_members_without_their_own_exit() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;
        // 前提：父号确实没有出口（与线上 #776 同构）。
        assert!(
            parent_snapshot(&svc).proxy_url.is_none(),
            "构造前提：父号必须无代理"
        );

        let resp = svc
            .clone_credential(1, 2, None, None, None, None, None)
            .await
            .expect("加分身应成功");

        assert!(
            resp.message.contains("没有独立出口") && resp.message.contains("#1"),
            "同 key 的 #1 无出口必须被点名告警，实际: {}",
            resp.message
        );
        // ⭐ 承重：告警不得升级成"自动改配置"。
        assert!(
            parent_snapshot(&svc).proxy_url.is_none(),
            "父号的 proxy_url 是显式配置，克隆路径只许告警、绝不许写它，实际 {:?}",
            parent_snapshot(&svc).proxy_url
        );
        // 新建的两份该拿到节点 —— 否则"父号无出口"这句可能只是因为整池都没分到。
        let ids = resp.credential_ids.expect("多开必须下发全部 id");
        let urls = proxy_urls_by_id(&svc, &ids);
        assert!(
            urls.iter().all(|u| u.is_some()),
            "两个节点两份，应各自拿到出口，实际 {urls:?}"
        );
    }

    /// 🔴 承重（任务一的判据）：查找必须按 **key**，不能按 `cloneGroup`。
    ///
    /// 这是缺陷能长期存活的原因：同账号里最先入池的那一份**天然没有组标识**
    /// （组是后来加分身才产生的），按组去找就恰好漏掉它 —— 而它正是那个裸 IP。
    ///
    /// 构造让两种判据结果不同：父号 `#1` 无 `cloneGroup`，新建的份拿到一个新组。
    /// 按 key 查 → 找到 `#1` → 告警；按组查 → `#1` 不在任何组里 → 静默。
    ///
    /// ⚠️ 这条能成立依赖**顺序**：名单必须在组标识回填**之前**取。回填之后父号也在组里了，
    /// 两种判据就再也分不出来（那正是本仓「测了分支内部、没测分支顺序」的老毛病）。
    ///
    /// 回退即 FAILED：把 `same_key_peers` 改成按 `clone_group` 过滤，
    /// 或把回填那段挪到取名单之前 —— 告警消失，本条变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_bare_exit_lookup_keys_on_the_api_key_not_the_clone_group() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 1).await;
        // 前提：父号一个组标识都没有（线上 #776 就是这样）。
        assert!(
            parent_snapshot(&svc).clone_group.is_none(),
            "构造前提：父号必须无 cloneGroup"
        );

        let resp = svc
            .clone_credential(1, 1, None, None, None, None, None)
            .await
            .expect("加 1 份应成功");

        assert!(
            resp.message.contains("没有独立出口") && resp.message.contains("#1"),
            "父号无组标识时仍必须被发现（按组查会漏掉它，那就是本缺陷），实际: {}",
            resp.message
        );
    }

    /// 🔴 源码级守卫：**取名单**必须早于**回填组标识**。
    ///
    /// 为什么必须额外有这一条（这是本仓「测了分支内部、没测分支顺序」那一类的正解）：
    /// 上面那条按-key 行为测试的判别力**依赖这个顺序**。实测过：只把判据改成按组 →
    /// 那条测试红；但**同时**把回填提到取名单之前 → 它又变绿了（回填先把父号补进组里，
    /// 按组查也能查到）。也就是说没有本条守卫时，两处一起改就能让缺陷重新隐形。
    ///
    /// 回退即 FAILED：把回填那段挪到 `same_key_peers` 之前 —— 位置比较翻转，本条变红。
    #[test]
    fn the_same_key_peer_snapshot_must_be_taken_before_the_group_backfill() {
        let src = include_str!("service.rs");
        // needle 运行时拼接：写成字面量会被 include_str! 读到本测试自身，
        // 两个 find 都命中这里 → 顺序恒成立 → 断言静默作废（同 strict_node_check 那条）。
        let snapshot = format!("{}{}", "let same_key_peers = ", "new_cred");
        let backfill = format!(
            "{}{}",
            "for peer in same_key_peers.iter()", ".filter(|p| p.clone_group.is_none())"
        );
        let snapshot_at = src.find(snapshot.as_str()).expect("同 key 名单快照应存在");
        let backfill_at = src.find(backfill.as_str()).expect("组标识回填应存在");
        assert!(
            snapshot_at < backfill_at,
            "取名单（位置 {snapshot_at}）必须早于回填（位置 {backfill_at}）：\
             反过来会让「按 key 查」与「按组查」再也无法区分，判据被改坏也测不出来"
        );
    }

    /// 对照组：同 key 的成员**都有**独立出口时不得告警。
    ///
    /// 没有这一条，上面两条可以靠"永远告警"蒙过去 —— 而永远告警等于没有告警
    /// （用户会学会忽略它），本仓已有多起同类文案失效。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_warning_when_every_same_key_member_already_has_an_exit() {
        let svc = mk_clone_service();
        let nodes = seed_nodes(&svc, 3).await;
        // 父号自己先绑一个节点（面板上"给这一份配个出口"的等价操作）。
        svc.token_manager
            .set_credential_proxy(1, Some(node_url(0)), None, None)
            .expect("给父号绑节点应成功");
        assert_eq!(nodes.len(), 3);

        let resp = svc
            .clone_credential(1, 2, None, None, None, None, None)
            .await
            .expect("加分身应成功");

        assert!(
            !resp.message.contains("没有独立出口"),
            "同 key 全员有出口时不得告警（狼来了会让告警失效），实际: {}",
            resp.message
        );
        // 顺带钉住：父号已有的出口不得被本次克隆改掉。
        assert_eq!(
            parent_snapshot(&svc).proxy_url.as_deref(),
            Some(node_url(0).as_str()),
            "父号已配的出口不得被克隆路径覆盖"
        );
    }

    /// 🔴 承重（任务二）：回填后父号的 `cloneGroup` 与新建的份一致，
    /// 且**除它之外一个字段都没被动**。
    ///
    /// 为什么要回填：前端 `groupClones` 为「父号早于 cloneGroup 字段入池」维护了一整套
    /// `apiKeyHash` 回落分组。回填让**新产生的**数据不再欠这笔债（老数据仍靠回落兜住，
    /// 本轮刻意不删回落逻辑）。
    ///
    /// 为什么这与「不改父号 proxy_url」不矛盾：`cloneGroup` 是系统内部的分组标识，
    /// 没有语义选择余地（父号确实属于那个组）；`proxy_url` 是用户的显式配置。
    ///
    /// 回退即 FAILED：删掉 service 里那段 `set_clone_identity` 回填循环 —— 第一条变红。
    /// 把回填改成连 `clone_seq` 一起写（或顺手写别的字段）—— 第三条变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloning_backfills_the_clone_group_onto_the_same_key_parent() {
        let svc = mk_clone_service();
        seed_nodes(&svc, 2).await;
        let before = parent_snapshot(&svc);
        assert!(before.clone_group.is_none(), "构造前提：父号无 cloneGroup");

        let resp = svc
            .clone_credential(1, 2, None, None, None, None, None)
            .await
            .expect("加分身应成功");
        let ids = resp.credential_ids.expect("多开必须下发全部 id");

        let after = parent_snapshot(&svc);
        let group = after
            .clone_group
            .clone()
            .expect("父号必须被回填 cloneGroup（否则前端只能靠 apiKeyHash 回落分组）");
        // 与新建的每一份同组 —— 回填一个**不同**的 UUID 比不回填更糟（面板上裂成两组）。
        for id in &ids {
            let child = svc
                .token_manager
                .export_credential(*id)
                .expect("分身应存在");
            assert_eq!(
                child.clone_group.as_deref(),
                Some(group.as_str()),
                "分身 #{id} 必须与回填后的父号同组"
            );
        }

        // ⭐ 承重：回填**只**动 clone_group。逐字段比对（比挑几个字段断言更难被绕过）。
        assert_eq!(
            after.clone_seq, before.clone_seq,
            "回填不得给父号凭空编号（编号必须走 reserve_clone_seqs，否则与组内既有号撞车）"
        );
        let mut expected = before.clone();
        expected.clone_group = after.clone_group.clone();
        assert_eq!(
            serde_json::to_value(&expected).expect("序列化父号快照"),
            serde_json::to_value(&after).expect("序列化父号现状"),
            "回填只许改 cloneGroup，其它字段一个都不能动"
        );
    }
}

/// 测试用小助手：把 `Result<bool, _>` 当成断言用，避免每处都 unwrap。
#[cfg(test)]
trait AwaitOk {
    fn await_ok(self) -> bool;
}

#[cfg(test)]
impl AwaitOk for Result<bool, AdminServiceError> {
    fn await_ok(self) -> bool {
        self.expect("操作应成功")
    }
}

#[cfg(test)]
mod balance_baseline_tests {
    //! G-2：新取到的余额真值与「花费基线」必须**成对更新**。
    //!
    //! 断言的全是 `get_cached_balances()` 的输出（前端真正消费的那个端点），
    //! 不断言内部表长什么样。
    use super::balance_cache_tests::mk_service_with_one_credential;
    use super::super::*;

    fn mk_balance(remaining: f64, used: f64) -> BalanceResponse {
        BalanceResponse {
            id: 1,
            subscription_title: Some("Kiro Pro".to_string()),
            current_usage: used,
            usage_limit: 100.0,
            remaining,
            usage_percentage: used,
            next_reset_at: None,
            overage_enabled: false,
            overage_cap: 0.0,
            effective_limit: 100.0,
            stale: false,
            optimistic: false,
        }
    }

    /// 面板上那个号当前显示的 remaining。
    fn shown_remaining(svc: &AdminService, id: u64) -> f64 {
        svc.get_cached_balances()
            .balances
            .get(&id)
            .unwrap_or_else(|| panic!("凭据 #{id} 应有缓存余额"))
            .balance
            .remaining
    }

    /// ⭐ 回归（用户反馈「额度/积分刷新太慢/不对」）：取到新真值后**不得再扣一次**已花掉的量。
    ///
    /// # 旧代码为何 FAIL
    ///
    /// `get_balance` 只 `cache.insert` 而不动基线 ⇒ 新真值（已含那 20）配着旧基线（50）
    /// ⇒ 面板再扣一次 delta=70-50=20 ⇒ 显示 60 而真值是 80。
    /// 把 `commit_fresh_balance` 里的 `push_balance_snapshots_to_scheduler` 那行删掉
    /// （= 回到旧行为），本测试最后一条断言必 FAILED（拿到 60）。
    #[test]
    fn fresh_truth_resets_the_spend_baseline_so_it_is_not_double_counted() {
        let svc = mk_service_with_one_credential();
        let key = svc.balance_cache_key(1);

        // t0：拿到真值 remaining=100，此刻本地累计花费 50
        svc.token_manager.add_credits(1, 50.0);
        svc.commit_fresh_balance(key.clone(), mk_balance(100.0, 0.0));
        assert_eq!(shown_remaining(&svc, 1), 100.0, "刚取到真值时不应有修正");

        // 期间花掉 20 → 乐观修正把它扣掉（这是既有的、正确的行为）
        svc.token_manager.add_credits(1, 20.0);
        assert_eq!(
            shown_remaining(&svc, 1),
            80.0,
            "两次真值之间应按本地花费乐观推进"
        );

        // 用户点「查看余额」，上游返回的真值 80 **已经包含**那 20。
        svc.commit_fresh_balance(key, mk_balance(80.0, 20.0));
        assert_eq!(
            shown_remaining(&svc, 1),
            80.0,
            "新真值已含那 20，绝不能再扣一次（旧代码在这里给出 60）"
        );
    }

    /// 只有**本次取到真值**的账号才重置基线；其余账号保留原基线。
    ///
    /// # 旧代码为何 FAIL
    ///
    /// 原 `push_balance_snapshots_to_scheduler` 无条件把所有账号的基线推到"现在"。
    /// 于是刷新失败（缓存仍是旧真值）的号，其"缓存之后已花掉的量"被一次性抹掉 ⇒
    /// 面板与调度器都把它当成比实际更有余额的号。
    /// 把 `fresh_keys` 判断改回无条件 `used_now`，第二条断言必 FAILED（拿到 100）。
    #[test]
    fn non_fresh_accounts_keep_their_baseline() {
        let svc = mk_service_with_one_credential();
        let key = svc.balance_cache_key(1);

        svc.token_manager.add_credits(1, 50.0);
        svc.commit_fresh_balance(key, mk_balance(100.0, 0.0));
        svc.token_manager.add_credits(1, 30.0);
        assert_eq!(shown_remaining(&svc, 1), 70.0);

        // 模拟「本轮该号刷新失败」的收尾回推：fresh_keys 为空。
        svc.push_balance_snapshots_to_scheduler(&HashSet::new());
        assert_eq!(
            shown_remaining(&svc, 1),
            70.0,
            "没取到新真值的号必须保留原基线，否则已花掉的 30 被抹掉、显示回 100"
        );
    }

    /// 源码守卫：`get_balance` 不得再内联 `cache.insert`（那会绕过基线重置）。
    ///
    /// 这条锁的是**接线**而非逻辑：上面两条测的是 `commit_fresh_balance` 的行为，
    /// 但真正的用户路径是 `get_balance`；若哪天有人在那里又写回一个裸 insert，
    /// 行为测试全绿而缺陷回归。单测无法真跑 `get_balance`（要打 app.kiro.dev），故用源码断言。
    #[test]
    fn get_balance_writes_through_the_single_commit_path() {
        let src = include_str!("balance_cache.rs");
        let body = src
            .split("pub async fn get_balance")
            .nth(1)
            .expect("get_balance 不应被改名")
            .split("fn balance_cache_key")
            .next()
            .expect("balance_cache_key 应紧随其后");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let commit = format!("self.commit_fresh{}", "_balance(");
        assert!(
            body.contains(commit.as_str()),
            "get_balance 必须走 commit_fresh_balance 收口（它负责同步重置花费基线）"
        );
        let inline_insert = format!("cache.insert{}", "(");
        assert!(
            !body.contains(inline_insert.as_str()),
            "get_balance 里不得内联 cache.insert —— 那会漏掉基线重置，面板把已花掉的量扣两次"
        );
    }

    /// 后台温和刷新同样必须走那个收口（同一漏改面）。
    #[test]
    fn background_refresh_writes_through_the_single_commit_path() {
        let src = include_str!("balance_cache.rs");
        let body = src
            .split("pub async fn refresh_all_balances_gently")
            .nth(1)
            .expect("refresh_all_balances_gently 不应被改名")
            .split("fn commit_fresh_balance")
            .next()
            .expect("commit_fresh_balance 应紧随其后");
        let commit = format!("self.commit_fresh{}", "_balance(");
        assert!(
            body.contains(commit.as_str()),
            "后台刷新也必须走 commit_fresh_balance（两条路径各写一份 insert 正是漏改根源）"
        );
    }

    /// `force` 查询串契约：**省略必须是 false**（老前端不带该参数时保持走缓存的原语义）。
    ///
    /// 走真实的 axum `Query` 提取器而不是直接反序列化 —— 要锁的正是"没带这个参数的请求
    /// 不会 400、且不会变成强制打上游"。回退即 FAIL：去掉 `#[serde(default)]` →
    /// 第一条断言（无查询串）直接解析失败。
    #[test]
    fn balance_query_force_defaults_to_false() {
        use crate::admin::handlers::BalanceQuery;
        use axum::extract::Query;

        let bare: Query<BalanceQuery> = Query::try_from_uri(
            &"http://x/api/admin/credentials/1/balance"
                .parse::<axum::http::Uri>()
                .unwrap(),
        )
        .expect("不带查询串的请求必须能解析（老前端就是这么发的）");
        assert!(!bare.0.force, "省略 force 必须走缓存（不改既有行为）");

        let forced: Query<BalanceQuery> = Query::try_from_uri(
            &"http://x/api/admin/credentials/1/balance?force=true"
                .parse::<axum::http::Uri>()
                .unwrap(),
        )
        .expect("解析 force=true");
        assert!(forced.0.force);
    }
}

#[cfg(test)]
mod cleanup_disabled_tests {
    //! 批量清理已禁用凭据（G-1）。
    //!
    //! 承重点不是"能删"，而是**该不该删的判据**：误清一个代挂号 =
    //! 删掉用户自配的第三方中转。所以每条排除都有一条对照断言。
    use super::super::*;

    /// 造一条凭据。`base_url` 非 None 即为代挂号（`is_custom_api_credential` 的旧数据判据）。
    fn mk(
        id: u64,
        auth_method: &str,
        base_url: Option<&str>,
        disabled: bool,
        reason: Option<DisabledReason>,
    ) -> KiroCredentials {
        // QuotaExceeded 死号带**当月**判定时刻：启动跨月恢复只放过期月份（缺失时间戳
        // 也视为可恢复），不带时刻会让这些号在构造时被自动复活，测试前提被打破。
        let quota_exhausted_ts = (reason == Some(DisabledReason::QuotaExceeded))
            .then(|| Utc::now().to_rfc3339());
        KiroCredentials {
            id: Some(id),
            auth_method: Some(auth_method.to_string()),
            // `.invalid` 是 RFC 6761 保留 TLD，保证永不解析 —— 测试不依赖本机 DNS
            // （历史事故：fake-IP 模式代理把 example.com 解到 198.18/16，被 SSRF 正确拦掉）。
            base_url: base_url.map(|s| s.to_string()),
            kiro_api_key: match auth_method {
                "api_key" | "custom_api" => Some(format!("ksk_test_{id}")),
                _ => None,
            },
            disabled,
            disabled_reason: reason,
            quota_exhausted_at: quota_exhausted_ts,
            ..Default::default()
        }
    }

    fn mk_service(creds: Vec<KiroCredentials>) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                // 单凭据格式 ⇒ persist 是 no-op，删除只走内存 + 内存回收站。
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 判据本身：四道排除各一条，加上"该清的真会被清"的对照。
    ///
    /// 回退即 FAIL：删掉 `cleanup_verdict` 里的 `is_custom_api` 分支 → 第 1 条失败；
    /// 删掉禁用原因那道 → 第 2/3 条失败；删掉可自愈那道 → 第 4 组失败。
    #[test]
    fn verdict_excludes_custom_api_and_passthrough_reasons() {
        // 代挂号：无论禁用原因是什么都不清
        assert_eq!(cleanup_verdict(Some(true), None), Some("custom_api"));
        assert_eq!(
            cleanup_verdict(Some(true), Some("QuotaExceeded")),
            Some("custom_api"),
            "代挂号即便原因看着像死号也不清（它的额度是中转站的，充值即可用）"
        );

        // 代挂专属原因：即便认不出是代挂（历史数据缺 auth_method/base_url）也要拦住
        assert_eq!(
            cleanup_verdict(Some(false), Some("PassthroughFailed")),
            Some("passthrough_disabled")
        );
        assert_eq!(
            cleanup_verdict(Some(false), Some("PassthroughOverloaded")),
            Some("passthrough_disabled")
        );

        // 可自愈原因：号会自己回池，删它等于拿走健康号
        for r in [
            "TooManyFailures",
            "SuspiciousActivityAuto",
            "TooManyRefreshFailures",
        ] {
            assert_eq!(
                cleanup_verdict(Some(false), Some(r)),
                Some("self_healable"),
                "{r} 在自愈白名单里，禁用态是瞬时的，不能当死号删"
            );
        }

        // 竞态：号已不在池里 → 不清，且原因不能报成代挂
        assert_eq!(cleanup_verdict(None, None), Some("not_in_pool"));
        assert_eq!(
            cleanup_verdict(None, Some("QuotaExceeded")),
            Some("not_in_pool"),
            "拿不到凭据时其余判据全是猜的，只能报'号没了'"
        );

        // 对照：真死号该清
        for r in [
            "Manual",
            "QuotaExceeded",
            "AccountSuspended",
            "InvalidRefreshToken",
            "InvalidConfig",
            "RequestLimitReached",
            "RegionProbeFailed",
            "RegionProbeTokenDead",
        ] {
            assert_eq!(
                cleanup_verdict(Some(false), Some(r)),
                None,
                "{r} 是 Kiro 号的死因（不在自愈白名单里），必须被清"
            );
        }
        // 禁用但无原因（老数据）也该清 —— 它已经是禁用态，本来就不参与调度。
        assert_eq!(cleanup_verdict(Some(false), None), None);
    }

    /// ⭐ 可自愈集合必须与 `token_manager::is_self_healable_reason` 的白名单**逐字符相同**。
    ///
    /// 那个函数是私有的、且吃枚举，这里没法直接调它，所以抄了一份。抄本会漂，而漂移的后果是
    /// 静默的：白名单加了新变体、这里没跟 → 那种号又会被当死号删走（正是本轮修的 bug）。
    ///
    /// 用**穷举 match** 而不是列表相等来锁：`DisabledReason` 新增变体时这条 match
    /// 会编译不过，逼作者当场判断"新原因可不可自愈"，而不是等线上删错号。
    ///
    /// 回退即 FAIL：从 `CLEANUP_SELF_HEALABLE_REASONS` 里删掉任一项 → 对应断言失败。
    #[test]
    fn self_healable_set_matches_token_manager_whitelist() {
        // 穷举全部变体，逐个声明期望值。expected 的取值依据是
        // `token_manager.rs::is_self_healable_reason` 的 matches! 白名单。
        let all: [(DisabledReason, bool); 14] = [
            (DisabledReason::Manual, false),
            (DisabledReason::TooManyFailures, true),
            (DisabledReason::TooManyRefreshFailures, true),
            (DisabledReason::QuotaExceeded, false),
            (DisabledReason::AccountSuspended, false),
            (DisabledReason::SuspiciousActivityAuto, true),
            (DisabledReason::InvalidRefreshToken, false),
            (DisabledReason::InvalidConfig, false),
            (DisabledReason::RequestLimitReached, false),
            (DisabledReason::PassthroughFailed, false),
            (DisabledReason::PassthroughOverloaded, false),
            (DisabledReason::RegionProbeFailed, false),
            (DisabledReason::RegionProbeTokenDead, false),
            (DisabledReason::Unknown, false),
        ];
        // 编译期门禁：新增变体后这个 match 缺分支即编译失败，届时必须回到上面的表里补一行。
        for (r, _) in &all {
            match r {
                DisabledReason::Manual
                | DisabledReason::TooManyFailures
                | DisabledReason::TooManyRefreshFailures
                | DisabledReason::QuotaExceeded
                | DisabledReason::AccountSuspended
                | DisabledReason::SuspiciousActivityAuto
                | DisabledReason::InvalidRefreshToken
                | DisabledReason::InvalidConfig
                | DisabledReason::RequestLimitReached
                | DisabledReason::PassthroughFailed
                | DisabledReason::PassthroughOverloaded
                | DisabledReason::RegionProbeFailed
                | DisabledReason::RegionProbeTokenDead
                | DisabledReason::Unknown => {}
            }
        }

        for (reason, healable) in all {
            assert_eq!(
                CLEANUP_SELF_HEALABLE_REASONS.contains(&reason),
                healable,
                "{} 的可自愈判定与 token_manager 白名单不一致",
                reason.as_str()
            );
        }
    }

    /// 判据里那两个字符串必须与 `DisabledReason::as_str()` 同源。
    ///
    /// 回退即 FAIL：把 `cleanup_verdict` 里的枚举调用换成手写字面量
    /// （例如 `"passthroughFailed"` 这种 camelCase 拼法）→ 本测试的 as_str 对不上。
    /// 这条锁的是**契约同源**：`as_str` 的字面量就是 Admin API 下发给前端的值，
    /// 而快照给我们的 `disabled_reason` 正是它的产物，两侧一旦分叉，排除会静默失效。
    #[test]
    fn passthrough_reason_strings_come_from_disabled_reason_as_str() {
        assert_eq!(
            cleanup_verdict(
                Some(false),
                Some(DisabledReason::PassthroughFailed.as_str())
            ),
            Some("passthrough_disabled")
        );
        assert_eq!(
            cleanup_verdict(
                Some(false),
                Some(DisabledReason::PassthroughOverloaded.as_str())
            ),
            Some("passthrough_disabled")
        );
    }

    /// ⭐ 端到端：真删一遍，代挂号必须**还在池里**、死号必须**进了回收站**。
    ///
    /// 这条测的是可观测状态（池 + 回收站），不是分支形状 —— 把
    /// `cleanup_disabled_credentials` 里的 `cleanup_verdict` 调用去掉（无条件收进候选），
    /// 第一组断言立刻 FAIL。
    #[test]
    fn cleanup_deletes_dead_kiro_credentials_and_keeps_passthrough() {
        let svc = mk_service(vec![
            // #1 未禁用 → 压根不是候选
            mk(1, "api_key", None, false, None),
            // #2 禁用的 Kiro 死号 → 清
            mk(
                2,
                "api_key",
                None,
                true,
                Some(DisabledReason::QuotaExceeded),
            ),
            // #3 管理员手动禁用的代挂号 → 留
            mk(
                3,
                "custom_api",
                Some("https://relay3.invalid/v1"),
                true,
                Some(DisabledReason::Manual),
            ),
            // #4 代挂号，但禁用原因是非代挂专属的未知值 → 仍靠 is_custom_api 拦住
            mk(
                4,
                "custom_api",
                Some("https://relay4.invalid/v1"),
                true,
                Some(DisabledReason::Unknown),
            ),
            // #5 认不出是代挂（api_key + 无 base_url），但原因是代挂专属 → 靠第二道网留
            mk(
                5,
                "api_key",
                None,
                true,
                Some(DisabledReason::PassthroughOverloaded),
            ),
            // #6 禁用无原因的老数据 → 清
            mk(6, "api_key", None, true, None),
            // #7 Kiro 号，但原因可自愈（自愈会把它复活）→ 留。删它 = 拿走健康号。
            mk(
                7,
                "api_key",
                None,
                true,
                Some(DisabledReason::TooManyFailures),
            ),
        ]);

        let resp = svc.cleanup_disabled_credentials(false);

        assert!(!resp.dry_run);
        assert_eq!(resp.disabled_total, 6, "#2..#7 共 6 个禁用号");
        assert_eq!(resp.candidates, vec![2, 6], "只有 #2/#6 是死号（且已升序）");
        assert_eq!(resp.deleted, 2);
        assert_eq!(resp.failed, 0);
        assert!(resp.results.iter().all(|r| r.ok));

        // 池里剩下：#1（未禁用）+ #3/#4/#5（代挂被排除）
        let remaining: Vec<u64> = {
            let mut v: Vec<u64> = svc
                .token_manager
                .snapshot()
                .entries
                .iter()
                .map(|e| e.id)
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(
            remaining,
            vec![1, 3, 4, 5, 7],
            "代挂号 #3/#4/#5 必须还在池里（它们不是死号，修好配置就能用）；\
             #7 是自愈途中的健康号，更不能删"
        );

        // 回收站里只有那两个死号 —— 「进回收站可恢复」而不是 purge。
        let mut trashed: Vec<u64> = svc.list_trash().trash.iter().map(|t| t.id).collect();
        trashed.sort_unstable();
        assert_eq!(trashed, vec![2, 6], "删掉的号必须进回收站（可恢复）");

        // skipped 逐条带原因，供前端解释"为什么这几个没删"
        let mut skipped: Vec<(u64, &str)> = resp.skipped.iter().map(|s| (s.id, s.reason)).collect();
        skipped.sort_unstable();
        assert_eq!(
            skipped,
            vec![
                (3, "custom_api"),
                (4, "custom_api"),
                (5, "passthrough_disabled"),
                (7, "self_healable"),
            ]
        );
    }

    /// dry-run 必须**一个号都不动**，但候选与真删完全一致（同一段筛选）。
    ///
    /// 回退即 FAIL：把 `if dry_run` 那道早返回删掉 → 预览会真删，池子少两个号。
    #[test]
    fn dry_run_reports_candidates_without_deleting() {
        let creds = vec![
            mk(
                1,
                "api_key",
                None,
                true,
                Some(DisabledReason::QuotaExceeded),
            ),
            mk(
                2,
                "custom_api",
                Some("https://relay.invalid/v1"),
                true,
                Some(DisabledReason::Manual),
            ),
            mk(
                3,
                "api_key",
                None,
                true,
                Some(DisabledReason::AccountSuspended),
            ),
        ];
        let svc = mk_service(creds);

        let preview = svc.cleanup_disabled_credentials(true);
        assert!(preview.dry_run);
        assert_eq!(preview.candidates, vec![1, 3]);
        assert_eq!(preview.deleted, 0, "预览不得删任何号");
        assert!(preview.results.is_empty(), "预览没有逐条删除结果");
        assert_eq!(svc.token_manager.total_count(), 3, "预览后池子必须原样");
        assert!(svc.list_trash().trash.is_empty(), "预览不得往回收站放东西");

        // 同一段筛选 ⇒ 真删的候选与预览逐字相同
        let real = svc.cleanup_disabled_credentials(false);
        assert_eq!(
            real.candidates, preview.candidates,
            "预览与真删必须同源（否则用户看到的和实际删的不是一回事）"
        );
        assert_eq!(real.deleted, 2);
    }

    /// 上限：超出部分**留给下一次**且在 skipped 里标 `over_limit`，不静默丢弃。
    ///
    /// 回退即 FAIL：去掉 `split_off` 那段 → 一次就把 201 个全删了，
    /// 第一条断言（deleted == 200）失败。
    #[test]
    fn cleanup_caps_at_limit_and_reports_the_rest() {
        let n = MAX_CLEANUP_DISABLED_IDS as u64 + 1;
        // 原因必须是**不可自愈**的死因，否则整批都会被 self_healable 那道排除掉，
        // 这条测的上限逻辑就一个候选都碰不到（测了个空）。
        let creds: Vec<KiroCredentials> = (1..=n)
            .map(|i| {
                mk(
                    i,
                    "api_key",
                    None,
                    true,
                    Some(DisabledReason::QuotaExceeded),
                )
            })
            .collect();
        let svc = mk_service(creds);

        let resp = svc.cleanup_disabled_credentials(false);
        assert_eq!(resp.disabled_total, n as usize);
        assert_eq!(resp.candidates.len(), MAX_CLEANUP_DISABLED_IDS);
        assert_eq!(resp.deleted, MAX_CLEANUP_DISABLED_IDS);
        // 升序截断 ⇒ 留下的必然是最大的那个 id（确定性，重复调用可收敛）
        assert_eq!(
            resp.skipped
                .iter()
                .filter(|s| s.reason == "over_limit")
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            vec![n]
        );
        assert_eq!(svc.token_manager.total_count(), 1, "只剩超出上限那一个");

        // 第二次调用把剩下那个清完 —— 这就是"留给下一次"的可收敛性。
        let again = svc.cleanup_disabled_credentials(false);
        assert_eq!(again.deleted, 1);
        assert_eq!(svc.token_manager.total_count(), 0);
    }

    /// ⭐ **顺序**断言：截断必须排在 `if dry_run` 早返**之前**。
    ///
    /// # 为什么单独测顺序，而不是各测一遍
    ///
    /// 「dry-run 会早返」和「超上限会截断」两个分支各自都是对的，现有测试也都覆盖了，
    /// 但它们**互相不知道对方存在**：把 `if dry_run` 那段整块挪到 `split_off` 之前，
    /// 两条旧测试仍全绿（一条不超限、一条不 dry-run），而预览会报 201 个候选、
    /// 真删只删 200 —— 用户看到的和实际删的不是一回事，正是 dry-run 唯一要防的事。
    ///
    /// 所以这条的断言不是"分支内容对不对"，而是**同一个池上预览与真删的候选逐字相等**，
    /// 且这个池刻意造在上限边界上（201），只有顺序错了才会分叉。
    ///
    /// 回退即 FAIL：把 `if dry_run || candidates.is_empty()` 那个 return 块移到
    /// `candidates.sort_unstable()` 之前 → 预览候选变 201 个，第一条断言失败。
    #[test]
    fn truncation_happens_before_dry_run_early_return() {
        let n = MAX_CLEANUP_DISABLED_IDS as u64 + 1;
        let creds: Vec<KiroCredentials> = (1..=n)
            .map(|i| {
                mk(
                    i,
                    "api_key",
                    None,
                    true,
                    Some(DisabledReason::QuotaExceeded),
                )
            })
            .collect();
        let svc = mk_service(creds);

        let preview = svc.cleanup_disabled_credentials(true);
        assert_eq!(
            preview.candidates.len(),
            MAX_CLEANUP_DISABLED_IDS,
            "预览也必须先截断：报 201 个而真删 200 个，预览就骗人了"
        );
        assert_eq!(
            preview
                .skipped
                .iter()
                .filter(|s| s.reason == CLEANUP_SKIP_OVER_LIMIT)
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            vec![n],
            "预览必须把'留给下一次'的那条也标出来（否则用户不知道还得再点一次）"
        );
        assert_eq!(
            svc.token_manager.total_count(),
            n as usize,
            "预览不得动池子"
        );

        // 同一个池、同一段筛选 ⇒ 真删的候选与预览逐字相等。这才是顺序正确的可观测证据。
        let real = svc.cleanup_disabled_credentials(false);
        assert_eq!(
            real.candidates, preview.candidates,
            "预览与真删的候选必须逐字相同（上限边界上尤其如此）"
        );
        assert_eq!(real.deleted, MAX_CLEANUP_DISABLED_IDS);
    }

    /// `disabled_total` 的恒等式：`== candidates.len() + skipped.len()`，**含** over_limit 那批。
    ///
    /// 锁的是 `types.rs` 上那句文档（原注释写"非 over_limit 的条数"，与实现不符）。
    /// 前端拿它当"池里有多少禁用号"的分母，少算一批会显示错的数。
    ///
    /// 回退即 FAIL：把实现改成注释描述的样子（`disabled_total` 减去 over_limit 条数）
    /// → 第二条断言失败。
    #[test]
    fn disabled_total_counts_every_disabled_credential_including_over_limit() {
        let n = MAX_CLEANUP_DISABLED_IDS as u64 + 3;
        let creds: Vec<KiroCredentials> = (1..=n)
            .map(|i| {
                // 混入两条被排除的，保证恒等式不是"candidates 恰好等于全部"的巧合
                match i % 100 {
                    7 => mk(
                        i,
                        "custom_api",
                        Some("https://relay.invalid/v1"),
                        true,
                        Some(DisabledReason::Manual),
                    ),
                    _ => mk(
                        i,
                        "api_key",
                        None,
                        true,
                        Some(DisabledReason::QuotaExceeded),
                    ),
                }
            })
            .collect();
        let svc = mk_service(creds);

        let resp = svc.cleanup_disabled_credentials(true);
        assert_eq!(resp.disabled_total, n as usize, "池里所有禁用号都要计入");
        assert_eq!(
            resp.disabled_total,
            resp.candidates.len() + resp.skipped.len(),
            "恒等式：每个禁用号必然落进 candidates 或 skipped 之一"
        );
        assert!(
            resp.skipped
                .iter()
                .any(|s| s.reason == CLEANUP_SKIP_OVER_LIMIT),
            "这个池刻意超上限，必须真触发 over_limit（否则本条测了个空）"
        );
    }

    /// 空池 / 全是未禁用号：安静返回零，不报错也不删。
    #[test]
    fn nothing_to_clean_is_a_quiet_zero() {
        let svc = mk_service(vec![mk(1, "api_key", None, false, None)]);
        let resp = svc.cleanup_disabled_credentials(false);
        assert_eq!(resp.disabled_total, 0);
        assert!(resp.candidates.is_empty());
        assert!(resp.skipped.is_empty(), "未禁用号不进 skipped（否则噪音）");
        assert_eq!(resp.deleted, 0);
        assert_eq!(svc.token_manager.total_count(), 1);
    }

    /// 请求体契约：`{}` / 缺体 / `{"dryRun":true}` 三种都得能解。
    ///
    /// 回退即 FAIL：去掉 `#[serde(default)]` → 第一条（`{}`）解析失败，
    /// 而"不带任何参数直接清理"正是最常见用法。
    #[test]
    fn request_body_parses_camel_case_and_defaults_to_real_delete() {
        use crate::admin::types::CleanupDisabledRequest;
        let empty: CleanupDisabledRequest = serde_json::from_str("{}").expect("空体应能解析");
        assert!(
            !empty.dry_run,
            "缺字段必须是真删（与既有 force 同款保守语义）"
        );
        let preview: CleanupDisabledRequest =
            serde_json::from_str(r#"{"dryRun":true}"#).expect("camelCase 应能解析");
        assert!(preview.dry_run);
    }
}

#[cfg(test)]
mod reprobe_quota_relogin_tests {
    //! POST /credentials/{id}/reprobe-region、/credentials/disable-quota-exceeded、
    //! /credentials/{id}/relogin 三个新端点的行为测试与源码守卫。
    //!
    //! 能纯逻辑测的（筛选、Skipped 处置、OAuth 校验、复活）用真 service 行为测；
    //! 需要真实上游的（NoUsableRegion/TokenDead/AccountThrottled 探测判决）用源码守卫锁
    //! 「失败分支绝不触碰禁用态」—— 本仓铁律：测试不依赖网络。

    use super::super::*;
    use crate::admin::types::ReprobeRegionResponse;

    /// 造一条凭据（对齐 cleanup_disabled_tests::mk 的形状）。
    fn mk(id: u64, auth_method: &str, disabled: bool, reason: Option<DisabledReason>) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            auth_method: Some(auth_method.to_string()),
            kiro_api_key: match auth_method {
                "api_key" | "custom_api" => Some(format!("ksk_test_{id}")),
                _ => None,
            },
            // OAuth 类必须带 refresh_token（validate 路径要求；测试里不触发刷新，仅占位）
            refresh_token: match auth_method {
                "api_key" | "custom_api" => None,
                _ => Some(format!("rt-test-{id}")),
            },
            disabled,
            disabled_reason: reason,
            ..Default::default()
        }
    }

    fn mk_service(creds: Vec<KiroCredentials>) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                // 单凭据格式 ⇒ persist 是 no-op，测试只改内存。
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    fn balance(id: u64, remaining: f64) -> BalanceResponse {
        BalanceResponse {
            id,
            subscription_title: None,
            current_usage: 0.0,
            usage_limit: 100.0,
            remaining,
            usage_percentage: 0.0,
            next_reset_at: None,
            overage_enabled: false,
            overage_cap: 0.0,
            effective_limit: 100.0,
            stale: false,
            optimistic: false,
        }
    }

    fn disabled_of(svc: &AdminService, id: u64) -> (bool, Option<String>) {
        let snap = svc.token_manager.snapshot();
        let e = snap
            .entries
            .iter()
            .find(|e| e.id == id)
            .expect("凭据应在池中");
        (e.disabled, e.disabled_reason.clone())
    }

    // ---------------- reprobe-region ----------------

    /// Skipped（已带 region）→ 原样返回当前 api_region，不算失败。
    /// 探测判据 `needs_api_region_probe` 对带 region 的 api_key 号直接 Skipped，零网络。
    #[tokio::test]
    async fn reprobe_skipped_with_region_returns_current_region() {
        let mut cred = mk(1, "api_key", false, None);
        cred.api_region = Some("eu-central-1".to_string());
        let svc = mk_service(vec![cred]);
        let resp: ReprobeRegionResponse = svc.reprobe_api_region(1).await.expect("Skipped 不是失败");
        assert_eq!(resp.region.as_deref(), Some("eu-central-1"));
        assert!(resp.message.contains("无需探测"));
    }

    /// Skipped（OAuth 号，无 region 概念）→ region=None + 说明文案，仍算成功。
    #[tokio::test]
    async fn reprobe_skipped_oauth_returns_no_region() {
        let svc = mk_service(vec![mk(1, "social", false, None)]);
        let resp: ReprobeRegionResponse = svc.reprobe_api_region(1).await.expect("Skipped 不是失败");
        assert_eq!(resp.region, None);
        assert!(resp.message.contains("无需探测"));
    }

    /// 号不存在 → NotFound（错误路径，不能假装探测成功）。
    #[tokio::test]
    async fn reprobe_missing_credential_is_not_found() {
        let svc = mk_service(vec![]);
        let err = svc.reprobe_api_region(1).await.expect_err("不存在必须报错");
        assert!(matches!(err, AdminServiceError::NotFound { id: 1 }));
    }

    /// ⭐ 源码级守卫（承重）：探测失败判决（NoUsableRegion / TokenDead / AccountThrottled）
    /// 只能返错误，**绝不能**调用禁用处置 —— 服役号被禁会把好号打掉
    /// （启动回填教训，见 `probe_and_persist_api_region` 文档）。
    /// 行为测试测不到（三个失败判决都要真上游探测），故锁源码。
    #[test]
    fn reprobe_failure_arms_must_not_disable_credential() {
        let src = include_str!("service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let marker = format!("pub async fn reprobe_api_region{}", "(");
        let start = prod.find(&marker).expect("reprobe_api_region 不应被改名");
        let body_end = prod[start..]
            .find("\n    pub ")
            .map(|i| i + start)
            .unwrap_or(prod.len());
        let body = &prod[start..body_end];
        // 三个失败判决必须各自成臂（归因错误消息能区分「探不了」与「探过不行」）。
        for arm in ["NoUsableRegion =>", "TokenDead =>", "AccountThrottled =>"] {
            assert!(
                body.contains(arm),
                "失败判决 {arm} 必须显式处置（缺失会静默漏分支）"
            );
        }
        // 本函数体里**不允许**出现任何禁用收口调用（拼接 needle 防自匹配）。
        let disable_call = format!("mark_region_probe_failed{}", "(");
        assert!(
            !body.contains(&disable_call),
            "服役号重探失败不得走禁用处置（mark_region_probe_failed 只属于上号路径）"
        );
        let set_disabled_call = format!(".set_disabled{}", "(");
        assert!(
            !body.contains(&set_disabled_call),
            "服役号重探失败不得 set_disabled（会把好号打掉）"
        );
    }

    // ---------------- disable-quota-exceeded ----------------

    /// 核心筛选：remaining<=0 且启用 → 禁；healthy（remaining>0）→ 不动。
    #[test]
    fn quota_exceeded_disables_only_exhausted_enabled() {
        let svc = mk_service(vec![
            mk(1, "api_key", false, None),
            mk(2, "api_key", false, None),
            mk(3, "api_key", true, Some(DisabledReason::Manual)),
        ]);
        let key1 = svc.balance_cache_key(1);
        let key2 = svc.balance_cache_key(2);
        let key3 = svc.balance_cache_key(3);
        svc.commit_fresh_balance(key1, balance(1, 0.0));
        svc.commit_fresh_balance(key2, balance(2, 42.5));
        svc.commit_fresh_balance(key3, balance(3, 0.0));

        let resp = svc.disable_quota_exceeded();
        assert_eq!(resp.disabled, 1);
        assert_eq!(resp.failed, 0);
        assert_eq!(resp.list, vec![1]);
        // #1 被禁且原因是额度用尽（面板可读，不是 Manual）。
        assert_eq!(disabled_of(&svc, 1), (true, Some("QuotaExceeded".to_string())));
        // #2 余额充足：不碰。 #3 已禁用：不是候选（幂等）。
        assert_eq!(disabled_of(&svc, 2), (false, None));
        assert_eq!(disabled_of(&svc, 3), (true, Some("Manual".to_string())));
    }

    /// 代挂号（custom_api）即使缓存显示超额也**绝不**代禁 —— 它的额度是中转站自己的。
    #[test]
    fn quota_exceeded_never_disables_custom_api() {
        let svc = mk_service(vec![mk(10, "custom_api", false, None)]);
        svc.commit_fresh_balance(svc.balance_cache_key(10), balance(10, -5.0));

        let resp = svc.disable_quota_exceeded();
        assert_eq!(resp.disabled, 0);
        assert_eq!(resp.list, Vec::<u64>::new());
        assert_eq!(disabled_of(&svc, 10), (false, None));
    }

    /// 无缓存 / 缓存未命中 → 不是候选（零上游，绝不触发余额查询）。
    #[test]
    fn quota_exceeded_ignores_uncached() {
        let svc = mk_service(vec![mk(1, "api_key", false, None)]);
        let resp = svc.disable_quota_exceeded();
        assert_eq!(resp.disabled, 0);
        assert_eq!(resp.list, Vec::<u64>::new());
        assert_eq!(disabled_of(&svc, 1), (false, None));
    }

    // ---------------- relogin ----------------

    /// OAuth 号复活：禁用 + 惩罚态清零 + 重新启用（失败计数复位、原因清空）。
    #[test]
    fn relogin_revives_oauth_credential() {
        let svc = mk_service(vec![mk(5, "idc", false, None)]);
        // 先造一个「惩罚态深」的号：额度耗尽禁用会把 failure_count 拉到阈值。
        svc.token_manager.report_quota_exhausted(5);
        assert_eq!(disabled_of(&svc, 5), (true, Some("QuotaExceeded".to_string())));

        svc.relogin_oauth(5).expect("OAuth 号复活应成功");
        let (disabled, reason) = disabled_of(&svc, 5);
        assert!(!disabled, "复活后必须重新启用");
        assert_eq!(reason, None, "复活后禁用原因必须清空");
        let snap = svc.token_manager.snapshot();
        let entry = snap.entries.iter().find(|e| e.id == 5).expect("号应在池中");
        assert_eq!(entry.failure_count, 0, "复活必须重置失败计数");
    }

    /// api_key 号拒绝复活（它没有 refreshToken 生命周期概念），custom_api 同理。
    #[test]
    fn relogin_rejects_api_key_and_custom_api() {
        let svc = mk_service(vec![
            mk(1, "api_key", false, None),
            mk(2, "custom_api", false, None),
        ]);
        let err = svc.relogin_oauth(1).expect_err("api_key 号必须拒绝");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        let err = svc.relogin_oauth(2).expect_err("代挂号必须拒绝");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        // 拒绝时不得动状态。
        assert_eq!(disabled_of(&svc, 1), (false, None));
        assert_eq!(disabled_of(&svc, 2), (false, None));
    }

    /// 号不存在 → NotFound。
    #[test]
    fn relogin_missing_credential_is_not_found() {
        let svc = mk_service(vec![]);
        let err = svc.relogin_oauth(99).expect_err("不存在必须报错");
        assert!(matches!(err, AdminServiceError::NotFound { id: 99 }));
    }

    // ---------------- 路由存在性守卫 ----------------

    /// 三个新端点必须挂在鉴权路由树内（路径 → handler 绑定，空白不敏感）。
    /// 回退即 FAIL：删掉任一 `.route(..)` → 前端 404 且编译/测试都不报。
    #[test]
    fn new_endpoints_are_wired_in_router() {
        let router = include_str!("router.rs");
        // ⚠️ 判据必须对空白不敏感（rustfmt 会把长 .route(..) 拆成多行），
        // 折叠空白后再比 —— 与 `api_region_setter_endpoint_is_wired` 同款写法。
        let compact: String = router.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let routes = [
            format!(
                "\"/credentials/{{id}}/reprobe-region\",post(reprobe_credential_region{}",
                ")"
            ),
            format!(
                "\"/credentials/disable-quota-exceeded\",post(disable_quota_exceeded{}",
                ")"
            ),
            format!("\"/credentials/{{id}}/relogin\",post(relogin_oauth{}", ")"),
            // 2026-08-11 对抗审查 m4：refresh-token 路由此前无守卫（漏注册不红）。
            format!(
                "\"/credentials/{{id}}/refresh-token\",put(update_credential_refresh_token{}",
                ")"
            ),
        ];
        for route in routes {
            assert!(
                compact.contains(&route),
                "新端点必须注册进鉴权路由树：{}",
                route
            );
        }
    }
}

#[cfg(test)]
mod kam_export_tests {
    //! GET /credentials/export-kam 的导出行为测试。
    //!
    //! 解密语义：at-rest 加密在启动加载期由 `CredentialsConfig::load` →
    //! `maybe_decrypt_to_string` 统一解密，内存凭据即明文；导出直接复用内存明文，
    //! 本模块构造明文凭据进内存，断言「导出 = 明文直通」（不经任何加解密）。

    use super::super::*;

    /// 造一条 OAuth 类凭据（带 refresh_token 才可能进 KAM 导出）。
    fn mk(id: u64, auth_method: &str, has_rt: bool, disabled: bool) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            auth_method: Some(auth_method.to_string()),
            email: Some(format!("user{id}@example.com")),
            access_token: Some(format!("at-test-{id}")),
            refresh_token: if has_rt {
                Some(format!("rt-test-{id}"))
            } else {
                None
            },
            region: Some("eu-central-1".to_string()),
            machine_id: Some(format!("machine-{id}")),
            client_id: Some(format!("client-{id}")),
            client_secret: Some(format!("secret-{id}")),
            profile_arn: Some(format!("arn:aws:iam::1:role/r{id}")),
            expires_at: Some("2030-01-01T00:00:00Z".to_string()),
            priority: id as u32,
            disabled,
            ..Default::default()
        }
    }

    fn mk_service(creds: Vec<KiroCredentials>) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                // 单凭据格式 ⇒ persist 是 no-op，测试只改内存（同 reprobe 测试先例）。
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    /// 导出即内存明文直通：refreshToken / accessToken 与构造值逐字一致。
    /// 这是「at-rest 密文 → 明文出站」语义的落点（解密发生在加载期，此处零加解密）。
    #[test]
    fn export_includes_plaintext_tokens() {
        let svc = mk_service(vec![mk(1, "social", true, false)]);
        let resp = svc.export_kam_credentials(None);
        assert_eq!(resp.accounts.len(), 1);
        let acc = &resp.accounts[0];
        assert_eq!(acc.refresh_token.as_deref(), Some("rt-test-1"));
        assert_eq!(acc.access_token.as_deref(), Some("at-test-1"));
        assert_eq!(acc.client_secret.as_deref(), Some("secret-1"));
    }

    /// 字段映射对齐 KAM 1.8.3+ 平铺格式；idp 复用本仓 social → Google 的既有推断。
    #[test]
    fn export_maps_kam_fields() {
        let mut cred = mk(1, "social", true, true);
        cred.region = None;
        cred.auth_region = None;
        cred.api_region = Some("ap-southeast-1".to_string());
        let svc = mk_service(vec![cred]);
        let acc = &svc.export_kam_credentials(None).accounts[0];
        assert_eq!(acc.email.as_deref(), Some("user1@example.com"));
        assert_eq!(acc.idp.as_deref(), Some("Google"));
        assert_eq!(acc.auth_method.as_deref(), Some("social"));
        assert_eq!(acc.status.as_deref(), Some("disabled"));
        // region 回退链（MINOR-3 修正）：本用例 region/auth_region 均缺 → 落第三级
        // api_region（effective_upstream_region 与导出同源，实测覆盖三级链末端）
        assert_eq!(acc.region.as_deref(), Some("ap-southeast-1"));
        assert_eq!(acc.machine_id.as_deref(), Some("machine-1"));
        assert_eq!(acc.client_id.as_deref(), Some("client-1"));
        assert_eq!(acc.profile_arn.as_deref(), Some("arn:aws:iam::1:role/r1"));
        assert_eq!(acc.expires_at.as_deref(), Some("2030-01-01T00:00:00Z"));
    }

    /// region 回退链：region 为空时依次落到 auth_region / api_region。
    #[test]
    fn export_region_falls_back_through_chain() {
        let mut cred = mk(2, "social", true, false);
        cred.region = None;
        cred.auth_region = Some("us-west-2".to_string());
        cred.api_region = Some("ap-northeast-1".to_string());
        let svc = mk_service(vec![cred]);
        let acc = &svc.export_kam_credentials(None).accounts[0];
        assert_eq!(acc.region.as_deref(), Some("us-west-2"));
    }

    /// 无 refreshToken 的号（api_key / custom_api）KAM 无对应字段 → 整条跳过。
    #[test]
    fn export_skips_credentials_without_refresh_token() {
        let mut api = mk(1, "api_key", false, false);
        api.kiro_api_key = Some("ksk_test_1".to_string());
        let mut passthrough = mk(2, "custom_api", false, false);
        passthrough.api_key = Some("sk-pt-2".to_string());
        let svc = mk_service(vec![api, passthrough, mk(3, "social", true, false)]);
        let resp = svc.export_kam_credentials(None);
        assert_eq!(resp.accounts.len(), 1);
        assert_eq!(resp.accounts[0].email.as_deref(), Some("user3@example.com"));
    }

    /// 空池 → accounts 空数组，不报错。
    #[test]
    fn export_empty_pool_returns_empty_accounts() {
        let svc = mk_service(vec![]);
        let resp = svc.export_kam_credentials(None);
        assert!(resp.accounts.is_empty());
    }

    /// ids 过滤：仅导出集合内的 ID。
    #[test]
    fn export_respects_id_filter() {
        let svc = mk_service(vec![
            mk(1, "social", true, false),
            mk(2, "social", true, false),
            mk(3, "social", true, false),
        ]);
        let filter: HashSet<u64> = [1u64, 3].into_iter().collect();
        let resp = svc.export_kam_credentials(Some(&filter));
        let emails: Vec<&str> = resp
            .accounts
            .iter()
            .filter_map(|a| a.email.as_deref())
            .collect();
        assert_eq!(emails, vec!["user1@example.com", "user3@example.com"]);
    }

    /// 按 priority 升序（与 UI 列表一致）。
    #[test]
    fn export_sorted_by_priority() {
        let mut low = mk(1, "social", true, false);
        low.priority = 10;
        let mut high = mk(2, "social", true, false);
        high.priority = 1;
        let svc = mk_service(vec![low, high]);
        let resp = svc.export_kam_credentials(None);
        let emails: Vec<&str> = resp
            .accounts
            .iter()
            .filter_map(|a| a.email.as_deref())
            .collect();
        assert_eq!(emails, vec!["user2@example.com", "user1@example.com"]);
    }

    /// 序列化契约：camelCase 键名 + 平铺 refreshToken + 无 null 字段（KAM 导入器判型要求）。
    #[test]
    fn export_serialization_contract() {
        let svc = mk_service(vec![mk(1, "social", true, false)]);
        let json = serde_json::to_value(svc.export_kam_credentials(None)).expect("序列化应成功");
        let obj = json.as_object().expect("顶层应为对象");
        assert_eq!(obj["version"], "1.8.3");
        assert!(obj["exportedAt"].as_str().is_some_and(|s| !s.is_empty()));
        let acc = obj["accounts"][0].as_object().expect("账号应为对象");
        assert!(acc.contains_key("refreshToken"), "KAM 平铺契约要求 refreshToken 直接在账号对象上");
        assert!(!acc.contains_key("refresh_token"), "键名必须是 camelCase");
        for (k, v) in acc {
            assert!(!v.is_null(), "字段 {k} 不应为 null（None 字段应省略）");
        }
    }

    /// 路由存在性守卫：export-kam 端点必须挂在鉴权路由树内。
    /// 回退即 FAIL：删掉 `.route(..)` → 前端 404 且编译/测试都不报。
    #[test]
    fn export_kam_endpoint_is_wired_in_router() {
        let router = include_str!("router.rs");
        // 判据对空白不敏感（rustfmt 会把长 .route(..) 拆多行），折叠空白后再比。
        let compact: String = router.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let route = format!(
            "\"/credentials/export-kam\",get(export_kam_credentials{}",
            ")"
        );
        assert!(
            compact.contains(&route),
            "export-kam 端点必须注册进鉴权路由树：{route}"
        );
    }
}

#[cfg(test)]
mod config_write_tests {
    //! 配置写路径（update_config / import_config）的健壮性测试：
    //! 写锁结构守卫、备份轮换行为、字段级 diff 审计行为。
    use super::super::*;
    use crate::admin::types::UpdateConfigRequest;

    /// 测试用临时目录（Drop 时自动清理；panic 时由 OS 留着，带 pid 不与其他进程撞）。
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 🔴 承重：两个配置写路径必须持同一把写锁，且持锁先于临界区起点。
    ///
    /// 根除的是 lost update：并发两个 `update_config` 各自 load 后交错 save，
    /// 后完成者会把先完成者的改动整体覆盖（都改不同字段时静默吞掉先写字段）。
    /// 守卫锁死两件事：
    /// 1. `update_config` 包装函数必须先持锁、再委托 locked 实现（锁保护得住整个临界区）；
    /// 2. `import_config` 必须先持锁、再写盘（save 是临界区终点）。
    ///
    /// 回退即 FAIL：把持锁语句从函数里挪走 / 移到委托调用之后 / 换锁名。
    #[test]
    fn config_write_lock_covers_both_write_paths() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let lock = format!("config_write{}", "_lock.lock()");
        let count = prod.matches(&lock).count();
        assert!(
            count >= 2,
            "两个写路径必须各持一次锁（当前 {count} 处）"
        );

        // update_config 包装函数：持锁在委托调用之前
        let update_fn = format!("pub fn update_config{}", "(");
        let uf = prod
            .find(&update_fn)
            .expect("update_config 包装函数不该被改名");
        let body_end = prod[uf..]
            .find("\n    pub fn ")
            .map(|i| i + uf)
            .unwrap_or(prod.len());
        let body = &prod[uf..body_end];
        let li = body
            .find(&lock)
            .expect("update_config 必须先持写锁，否则并发 save 互相覆盖");
        let call = format!("self.update_config{}", "_locked(req)");
        let ci = body
            .find(&call)
            .expect("update_config 必须委托给锁内实现");
        assert!(li < ci, "持锁必须在委托调用之前，否则保护不到临界区");

        // import_config：持锁在写盘之前
        let import_fn = format!("pub fn import_config{}", "(");
        let ii = prod
            .find(&import_fn)
            .expect("import_config 不该被改名");
        let iend = prod[ii..]
            .find("\n    pub fn ")
            .map(|i| i + ii)
            .unwrap_or(prod.len());
        let ibody = &prod[ii..iend];
        // 写盘调用是跨行链式（`imported\n .save()`），折叠空白再比（同 router 守卫写法）。
        let icompact: String = ibody.chars().filter(|c| !c.is_whitespace()).collect();
        let il = icompact
            .find(&lock)
            .expect("import_config 必须先持写锁，否则与并发更新互相覆盖");
        let save = format!("imported{}", ".save()");
        let si = icompact
            .find(&save)
            .expect("import_config 必须写盘保存");
        assert!(il < si, "持锁必须在写盘之前，否则并发导入相互覆盖");
    }

    /// 备份轮换保留 3 代（.bak 最新 → .bak.1 → .bak.2 最旧），当前文件原位不动。
    #[test]
    fn rotate_config_backup_keeps_three_generations() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_cfg_bak_{}",
            std::process::id()
        )));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        let cfg = dir.0.join("config.json");

        std::fs::write(&cfg, b"v0").unwrap();
        rotate_config_backup(&cfg);
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak")).unwrap(),
            "v0"
        );

        std::fs::write(&cfg, b"v1").unwrap();
        rotate_config_backup(&cfg);
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak.1")).unwrap(),
            "v0"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak")).unwrap(),
            "v1"
        );

        std::fs::write(&cfg, b"v2").unwrap();
        rotate_config_backup(&cfg);
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak.2")).unwrap(),
            "v0"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak.1")).unwrap(),
            "v1"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.with_extension("json.bak")).unwrap(),
            "v2"
        );
        // 当前文件保持最新内容未被轮换动过
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "v2");
    }

    /// 字段级 diff 审计：只记字段名/路径，绝不记字段值（敏感字段的值因此不会进日志）。
    #[test]
    fn diff_json_fields_reports_names_not_values() {
        // 值变了 → 记字段名；旧值/新值本身绝不出现
        let old = serde_json::json!({ "apiKey": "secret-old", "port": 8080 });
        let new = serde_json::json!({ "apiKey": "secret-new", "port": 8080 });
        let d = diff_json_fields(&old, &new);
        assert_eq!(d, vec!["apiKey".to_string()]);
        assert!(
            d.iter().all(|p| !p.contains("secret")),
            "diff 结果只能有字段名，不能夹带任何字段值"
        );

        // 完全相同 → 空
        let same = serde_json::json!({ "a": 1, "b": { "c": [1, 2] } });
        assert!(diff_json_fields(&same, &same).is_empty());

        // 新增/删除键 → 记路径
        let a = serde_json::json!({ "x": 1 });
        let b = serde_json::json!({ "x": 1, "y": 2 });
        assert_eq!(diff_json_fields(&a, &b), vec!["y".to_string()]);
        assert_eq!(diff_json_fields(&b, &a), vec!["y".to_string()]);

        // 嵌套对象 → 递归记完整路径
        let o1 = serde_json::json!({ "outer": { "inner": 1 } });
        let o2 = serde_json::json!({ "outer": { "inner": 2 } });
        assert_eq!(
            diff_json_fields(&o1, &o2),
            vec!["outer.inner".to_string()]
        );

        // 整块结构替换（数组 / 标量类型变）→ 记顶层路径
        let m1 = serde_json::json!({ "arr": [1, 2] });
        let m2 = serde_json::json!({ "arr": [3] });
        assert_eq!(diff_json_fields(&m1, &m2), vec!["arr".to_string()]);
    }

    // ============ update_config_locked 行为测试（2026-08-15 补）============
    //
    // 此前只有源码守卫（旧注释自称「单测无法真跑 update_config（需要真实 TokenManager +
    // 磁盘 config），故用源码断言」——前提其实不成立）：tmp 目录 + 写盘 config.json +
    // `Config::load` 带回 config_path 即可构造真实可跑的更新链路
    // （load → 逐字段改 → save → reload_config）。这批测试钉的是守卫钉不住的行为：
    // 字段 merge 不丢、restart_fields 累积、非法值整单拒绝且零写盘、
    // TIER1/TIER3 立即生效文案、error_messages per-key merge。

    /// 构造带磁盘 config.json 的 AdminService。
    ///
    /// seed 按测试意图写初始配置，整份写盘后经 `Config::load` 读回
    /// （与 update_config_locked 内部同一条加载路径），config_path 因此有值。
    fn svc_with_disk_config(
        dir: &TempDir,
        seed: impl FnOnce(&mut crate::model::config::Config),
    ) -> (Arc<AdminService>, std::path::PathBuf) {
        let path = dir.0.join("config.json");
        // 目录必须显式创建（TempDir 只登记路径不建目录）：缺了这行
        // fs::write 直接 NotFound panic，5 个 update_config 测试全红。
        std::fs::create_dir_all(&dir.0).unwrap();
        let mut cfg = crate::model::config::Config::default();
        seed(&mut cfg);
        std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
        let loaded = crate::model::config::Config::load(&path).expect("初始配置必须可加载");
        let tm = Arc::new(
            MultiTokenManager::new(loaded, vec![], None, None, false).expect("构造 token manager"),
        );
        (Arc::new(AdminService::new(tm, Vec::<String>::new())), path)
    }

    fn disk_config_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap())
            .expect("磁盘配置必须是合法 JSON")
    }

    fn bak_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".bak"))
            .collect();
        names.sort();
        names
    }

    /// 合法导入写盘后重读字段（config_path 从 token_manager 继承，serde skip 不进 JSON）。
    ///
    /// 必须 multi_thread：成功路径 `save` 在 runtime 内走 `block_in_place`，且
    /// `respawn_balance_task` 无条件 `tokio::spawn` socks 健康任务。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_config_writes_disk_and_rereads_field() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_imp_write_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "old.example.com".to_string();
            c.port = 8080;
        });
        assert!(
            bak_names(&dir.0).is_empty(),
            "前置：种子目录不得已有备份"
        );

        let resp = svc
            .import_config(serde_json::json!({
                "host": "imported.example.com",
                "port": 8080,
            }))
            .expect("合法导入必须成功（config_path 从 token_manager 继承）");
        assert!(resp.success);

        let disk = disk_config_json(&path);
        assert_eq!(
            disk["host"], "imported.example.com",
            "导入后重读磁盘必须看到改过的非敏感字段"
        );

        let bak = path.with_extension("json.bak");
        assert!(bak.exists(), "成功路径才轮换备份");
        let bak_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&bak).unwrap()).unwrap();
        assert_eq!(
            bak_json["host"], "old.example.com",
            "备份必须是导入前的磁盘内容"
        );
    }

    /// 非法 payload / 缺路径失败时不得轮换 .bak（rotate 只在校验全过且路径已回填之后）。
    #[test]
    fn import_config_invalid_payload_does_not_rotate_backup() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_imp_norot_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "h1".to_string();
            c.port = 8080;
        });
        let bak = path.with_extension("json.bak");
        std::fs::write(&bak, b"seed-bak").unwrap();
        let before_names = bak_names(&dir.0);
        let before_bak = std::fs::read(&bak).unwrap();
        let before_cfg = std::fs::read(&path).unwrap();

        let cases = [
            serde_json::json!("not-an-object"),
            serde_json::json!({ "host": "", "port": 8080 }),
            serde_json::json!({ "host": "h1", "port": 0 }),
            serde_json::json!({
                "host": "h1",
                "port": 8080,
                "errorMessages": { "x": { "type": "billing_error" } }
            }),
        ];
        for payload in cases {
            let err = svc
                .import_config(payload)
                .expect_err("非法 payload 必须拒绝");
            assert!(
                matches!(err, AdminServiceError::InvalidCredential(_)),
                "非法 payload 必须 InvalidCredential，实际: {err:?}"
            );
            assert_eq!(bak_names(&dir.0), before_names, "失败路径不得轮换 .bak");
            assert_eq!(
                std::fs::read(&bak).unwrap(),
                before_bak,
                ".bak 内容不得被轮换改写"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before_cfg,
                "失败路径零写盘"
            );
        }

        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![],
                None,
                None,
                false,
            )
            .expect("no-path token manager"),
        );
        let no_path = Arc::new(AdminService::new(tm, Vec::<String>::new()));
        let err = no_path
            .import_config(serde_json::json!({ "host": "h1", "port": 8080 }))
            .expect_err("缺路径必须拒绝");
        assert!(
            matches!(err, AdminServiceError::InternalError(_)),
            "缺路径应 InternalError，实际: {err:?}"
        );
        assert_eq!(bak_names(&dir.0), before_names, "缺路径不得轮换已有备份");
    }

    /// P1-8：导出 JSON / 配置快照不得包含 proxyUrl 内嵌密码。
    ///
    /// 回退即 FAIL：脱敏清单只删 `proxyPassword` 键却原样导出 `user:pass@host`。
    #[test]
    fn export_config_does_not_contain_proxy_url_password() {
        let password = format!("pxy-Pw-{}", "9f3e7a1c");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_exp_proxy_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, _path) = svc_with_disk_config(&dir, |c| {
            c.proxy_url = Some(format!("socks5://alice:{password}@127.0.0.1:1080"));
        });

        let exported = svc.export_config().expect("export 必须成功");
        let dumped = exported.to_string();
        assert!(
            !dumped.contains(&password),
            "导出 JSON 不得包含 proxyUrl 内嵌密码"
        );
        assert!(
            !exported
                .as_object()
                .expect("导出必须是对象")
                .contains_key("proxyPassword"),
            "脱敏清单必须省略 proxyPassword 键"
        );
        assert_eq!(
            exported["proxyUrl"], "socks5://127.0.0.1:1080",
            "导出 proxyUrl 必须剥掉 userinfo"
        );

        let snap = svc.get_config_snapshot();
        assert_eq!(
            snap.proxy_url.as_deref(),
            Some("socks5://127.0.0.1:1080"),
            "快照 proxyUrl 必须剥掉 userinfo"
        );
    }

    /// P1-8：`update_config` 写入时拆内嵌账密（与凭据上号同口径）。
    #[test]
    fn update_config_splits_proxy_url_embedded_credentials() {
        let password = format!("pxy-Pw-{}", "9f3e7a1c");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_proxy_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |_| {});

        let resp = svc
            .update_config(UpdateConfigRequest {
                proxy_url: Some(format!("socks5://alice:{password}@127.0.0.1:1080")),
                ..Default::default()
            })
            .expect("写入带 userinfo 的 proxyUrl 必须成功");
        assert!(resp.restart_required);
        for field in ["proxyUrl", "proxyUsername", "proxyPassword"] {
            assert!(
                resp.restart_fields.iter().any(|f| f == field),
                "拆账密后 {field} 必须进 restart_fields，实际 {:?}",
                resp.restart_fields
            );
        }

        let disk = disk_config_json(&path);
        assert_eq!(
            disk["proxyUrl"], "socks5://127.0.0.1:1080",
            "磁盘 proxyUrl 必须是剥掉账密后的干净 URL"
        );
        assert_eq!(disk["proxyUsername"], "alice");
        assert_eq!(disk["proxyPassword"], password.as_str());
        let disk_url = disk["proxyUrl"].as_str().unwrap_or("");
        assert!(
            !disk_url.contains(&password),
            "磁盘 proxyUrl 不得残留密码"
        );
    }

    /// P1-6：Windows OTA 自重启 bat 必须探 `/healthz`，失败时 `move` `.bak` 回原路径。
    #[test]
    fn windows_ota_relaunch_bat_contains_healthz_and_bak_rollback() {
        let bat = windows_relaunch_bat(
            "",
            r#"start "KiroStudio" "C:\ks\kirostudio.exe""#,
            r"C:\ks\kirostudio.exe",
            "http://127.0.0.1:8990/healthz",
        );
        assert!(bat.contains("/healthz"), "bat 必须循环探 /healthz");
        assert!(bat.contains(".bak"), "bat 失败路径必须使用 .bak 回滚");
        assert!(
            bat.contains("move /Y"),
            "bat 必须把 .bak move 回原 exe 路径"
        );
        assert!(
            bat.contains("curl.exe"),
            "bat 必须用 curl 探活（Win10+ 自带）"
        );
        assert_eq!(
            windows_healthz_probe_url("0.0.0.0", 8990),
            "http://127.0.0.1:8990/healthz"
        );

        let src = include_str!("service_restart.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let emit = format!("{}{}", "windows_relaunch_bat(&", "cwd_line");
        assert!(
            prod.contains(&emit),
            "spawn_windows_relaunch_process 必须调用 windows_relaunch_bat 落地脚本"
        );
        let probe = format!("{}{}", "windows_healthz_probe_url(", "listen_host");
        assert!(
            prod.contains(&probe),
            "落地 bat 的 /healthz URL 必须来自 listen_host/port，不能写死"
        );
    }

    /// 🔴 承重：改一个字段 → **只**改那一个，其余字段保持磁盘原值（merge 不丢）；
    /// 需重启字段按代码顺序累积进 restart_fields。
    ///
    /// 回退即 FAIL：把任一字段的写盘漏掉（「存了盘但读旧值」那类接线缺陷），
    /// 或 restart_fields 不按提交顺序 push（面板展示顺序错乱）。
    #[test]
    fn update_config_restart_fields_accumulate_and_unsubmitted_fields_preserved() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_restart_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "old.example.com".to_string();
            c.port = 8080;
            c.region = "us-east-1".to_string();
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                host: Some("new.example.com".to_string()),
                port: Some(9090),
                ..Default::default()
            })
            .expect("改 host+port 应成功");

        assert!(resp.restart_required, "host/port 都是重启字段");
        assert_eq!(
            resp.restart_fields,
            vec!["host".to_string(), "port".to_string()],
            "restart_fields 必须按代码顺序累积"
        );
        assert!(
            resp.message.contains("2 个字段"),
            "文案必须报 2 个字段需重启，实际: {}",
            resp.message
        );

        let disk = disk_config_json(&path);
        assert_eq!(disk["host"], "new.example.com", "提交的字段必须落盘");
        assert_eq!(disk["port"], 9090, "提交的字段必须落盘");
        assert_eq!(
            disk["region"], "us-east-1",
            "未提交的字段必须保持磁盘原值（merge 不丢）"
        );
    }

    /// 🔴 承重：非法值整单拒绝（Err），且**拒绝发生在写盘之前**——磁盘零改动。
    ///
    /// 覆盖四类校验：空串清洗后拒绝（host）、端口 0 拒绝（port）、
    /// 值域白名单拒绝（absorb_exhausted_status 只认 429/503）、枚举拒绝
    /// （load_balancing_mode 只认 priority/balanced）。
    ///
    /// 回退即 FAIL：把任一校验挪到 save 之后（拒绝但已落盘），或删掉任一校验，
    /// 对应断言失败。
    #[test]
    fn update_config_rejects_invalid_values_without_touching_disk() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_reject_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "h1".to_string();
            c.port = 8080;
        });
        let baseline = std::fs::read(&path).unwrap();

        let cases = [
            UpdateConfigRequest {
                host: Some("   ".to_string()),
                ..Default::default()
            },
            UpdateConfigRequest {
                port: Some(0),
                ..Default::default()
            },
            UpdateConfigRequest {
                upstream_retry_absorb_exhausted_status: Some(999),
                ..Default::default()
            },
            UpdateConfigRequest {
                load_balancing_mode: Some("bogus".to_string()),
                ..Default::default()
            },
        ];
        for req in cases {
            let err = svc
                .update_config(req)
                .expect_err("非法值必须整单拒绝");
            assert!(
                matches!(err, AdminServiceError::InvalidCredential(_)),
                "非法值拒绝必须用 InvalidCredential，实际: {err:?}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                baseline,
                "拒绝时必须零写盘（校验要先于 save）"
            );
        }
    }

    /// TIER1/TIER3 字段：保存后立即生效，不进 restart_fields、回「无需重启」。
    ///
    /// 用透传模拟缓存（TIER3 + setter 镜像）与吸收层开关（无 setter、只靠 reload_config
    /// 的 OR 链）各代表一类：两类都必须回「立即生效」——回「需重启」就是把热更字段
    /// 误分类的接线缺陷。
    #[test]
    fn update_config_hot_fields_report_immediate_effect_without_restart() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_hot_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.mock_cache_enabled = false;
            c.upstream_retry_absorb_enabled = false;
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                mock_cache_enabled: Some(true),
                upstream_retry_absorb_enabled: Some(true),
                ..Default::default()
            })
            .expect("热更字段应成功");
        assert!(!resp.restart_required, "热更字段不得要求重启");
        assert!(resp.restart_fields.is_empty());
        assert!(
            resp.message.contains("立即生效"),
            "热更字段必须回「立即生效」，实际: {}",
            resp.message
        );

        let disk = disk_config_json(&path);
        assert_eq!(disk["mockCacheEnabled"], true, "TIER3 字段必须落盘");
        assert_eq!(disk["upstreamRetryAbsorbEnabled"], true, "吸收层开关必须落盘");
    }

    /// 🔴 承重：userKey（apiKey）轮换走 `auth_keys` setter **即时生效、无需重启**。
    ///
    /// 旧行为：apiKey 进 restart_fields、面板提示「需重启」——重启会掐断在途流式请求。
    /// 现在应回「立即生效」且 auth_keys 立刻按新 key 判定（旧 key 立即失效）。
    ///
    /// ⚠️ auth_keys 是进程级全局 cell：本用例必须持 `auth_keys::test_serial()` 全程，
    /// 否则并行的其他用例（构造 AppState/AdminState 或改 key）会覆写同一份全局状态。
    /// 先播旧 key 模拟 main.rs 启动播种，再经 update_config 轮换 → 断言旧失效/新生效。
    #[test]
    fn update_config_user_key_hot_swaps_without_restart() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_user_key("sk-old")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_ukey_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.api_key = Some("sk-old".to_string());
        });
        assert!(
            crate::common::auth_keys::user_key_matches("sk-old"),
            "前置：播种后旧 key 应生效（模拟真实启动状态）"
        );

        let resp = svc
            .update_config(UpdateConfigRequest {
                api_key: Some("sk-new".to_string()),
                ..Default::default()
            })
            .expect("轮换 apiKey 应成功");
        assert!(!resp.restart_required, "apiKey 轮换不得要求重启");
        assert!(resp.restart_fields.is_empty(), "apiKey 不再进 restart_fields");
        assert!(
            resp.message.contains("立即生效"),
            "apiKey 轮换必须回「立即生效」，实际: {}",
            resp.message
        );

        // 鉴权活真相源立刻按新 key 判定：旧 key 失效、新 key 通过（热更定义）。
        assert!(
            crate::common::auth_keys::user_key_matches("sk-new"),
            "热更后新 apiKey 必须通过"
        );
        assert!(
            !crate::common::auth_keys::user_key_matches("sk-old"),
            "热更后旧 apiKey 必须立即失效"
        );

        let disk = disk_config_json(&path);
        assert_eq!(disk["apiKey"], "sk-new", "apiKey 必须落盘");
    }

    /// 承重：adminApiKey 轮换同样即时生效、无需重启。
    ///
    /// 语义上 admin key 是**新字段**（此前 UpdateConfigRequest 根本没有它，只能手改
    /// config.json + 重启）；现在走与 userKey 同款 setter 热更。自锁风险见字段注释。
    #[test]
    fn update_config_admin_key_hot_swaps_without_restart() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_admin_key("adm-old")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_akey_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.admin_api_key = Some("adm-old".to_string());
        });
        assert!(
            crate::common::auth_keys::admin_key_matches("adm-old"),
            "前置：播种后旧 key 应生效"
        );

        let resp = svc
            .update_config(UpdateConfigRequest {
                admin_api_key: Some("adm-new".to_string()),
                ..Default::default()
            })
            .expect("轮换 adminApiKey 应成功");
        assert!(!resp.restart_required, "adminApiKey 轮换不得要求重启");
        assert!(resp.restart_fields.is_empty());
        assert!(
            resp.message.contains("立即生效"),
            "adminApiKey 轮换必须回「立即生效」，实际: {}",
            resp.message
        );
        assert!(
            crate::common::auth_keys::admin_key_matches("adm-new"),
            "热更后新 adminApiKey 必须通过"
        );
        assert!(
            !crate::common::auth_keys::admin_key_matches("adm-old"),
            "热更后旧 adminApiKey 必须立即失效"
        );
        assert_eq!(disk_config_json(&path)["adminApiKey"], "adm-new", "必须落盘");
    }

    /// 空/空白 key 传空串 = 不改（防把手动写入 fail-closed 的意图和「手滑存空」混为一谈）。
    ///
    /// 只提交空白 apiKey/adminApiKey 时：不报错、不落盘、鉴权仍走旧 key（绝不清成空串，
    /// 清空 = fail-open 敞口；真正关闭通道的意图在 auth_keys 层由 setter 拒空兜底）。
    #[test]
    fn update_config_blank_key_is_ignored_not_wiped() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_user_key("sk-keep")
            .expect("启动播种（模拟 main.rs）不应失败");
        crate::common::auth_keys::set_admin_key("adm-keep")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_blank_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.api_key = Some("sk-keep".to_string());
            c.admin_api_key = Some("adm-keep".to_string());
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                api_key: Some("   ".to_string()),
                admin_api_key: Some("".to_string()),
                ..Default::default()
            })
            .expect("空白 key 应被忽略而非报错");
        assert!(
            resp.message.contains("无改动"),
            "空白 key 不算改动，实际: {}",
            resp.message
        );
        assert!(
            crate::common::auth_keys::user_key_matches("sk-keep"),
            "空白 key 不得清掉现有 apiKey"
        );
        assert!(
            crate::common::auth_keys::admin_key_matches("adm-keep"),
            "空白 key 不得清掉现有 adminApiKey"
        );
        let disk = disk_config_json(&path);
        assert_eq!(disk["apiKey"], "sk-keep");
        assert_eq!(disk["adminApiKey"], "adm-keep");
    }

    /// 🔴 承重：key 轮换与热字段**同批**提交时，reload_config 不得覆盖新 key。
    ///
    /// reload_config（token_manager）会把 apiKey/adminApiKey 这类 restart-only 字段
    /// 用 ArcSwap 旧值**钉回启动值**（split-brain 防护），鉴权却读 auth_keys 活单元——
    /// 所以 setter 必须放在 reload_config **之后**、以新值为准。本用例强制走
    /// mock_cache_enabled（TIER3 热字段 → 触发 reload_config）同批改 apiKey，
    /// 断言 reload 后 auth_keys 仍是新值：若有人把 setter 挪到 reload 之前或删了接线，
    /// 这里会当场红。
    #[test]
    fn update_config_key_survives_batched_hot_reload() {
        let _g = crate::common::auth_keys::test_serial();
        crate::common::auth_keys::set_user_key("sk-old")
            .expect("启动播种（模拟 main.rs）不应失败");
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_seq_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.api_key = Some("sk-old".to_string());
            c.mock_cache_enabled = false;
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                api_key: Some("sk-new".to_string()),
                // 热字段：确保本次更新触发 reload_config（正踩「顺序坑」的场景）。
                mock_cache_enabled: Some(true),
                ..Default::default()
            })
            .expect("key + 热字段同批应成功");
        assert!(!resp.restart_required);
        assert!(
            crate::common::auth_keys::user_key_matches("sk-new"),
            "reload_config 之后 setter 必须以新值为准，旧 key 不得复活"
        );
        assert!(
            !crate::common::auth_keys::user_key_matches("sk-old"),
            "reload 不得把钉回的旧启动值当成鉴权真值"
        );
        assert_eq!(disk_config_json(&path)["apiKey"], "sk-new");
    }

    /// 源码守卫：key 热更接线 + 「setter 必须在 reload_config 之后」的顺序不变量。
    ///
    /// 回退即 FAIL：
    /// - 删掉 update_config_locked 里的 set_user_key/set_admin_key 调用（接线断了，
    ///   key 轮换又退回重启生效）；
    /// - 把 setter 挪到 reload_config **之前**（reload 会把 key 钉回启动旧值，
    ///   顺序反了热更静默失效）。
    #[test]
    fn guard_update_config_seeds_auth_keys_after_reload() {
        let src = concat!(include_str!("config_update.rs"), include_str!("service.rs"));
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        for needle in [
            "crate::common::auth_keys::set_user_key",
            "crate::common::auth_keys::set_admin_key",
        ] {
            assert!(
                compact.contains(needle),
                "update_config 必须调 {needle} 热更（删接线 = key 轮换退回重启生效）"
            );
        }
        let reload = compact
            .find("self.token_manager.reload_config()")
            .expect("update_config 必须保留 reload_config 调用");
        let set_user = compact
            .find("crate::common::auth_keys::set_user_key")
            .expect("userKey setter 接线不该消失");
        let set_admin = compact
            .find("crate::common::auth_keys::set_admin_key")
            .expect("adminKey setter 接线不该消失");
        assert!(
            reload < set_user && reload < set_admin,
            "key setter 必须放在 reload_config 之后（reload 会把 key 钉回启动值，\
             顺序反了热更被 reload 覆盖而静默失效）"
        );
    }

    /// 提交与磁盘相同的值 → 「无改动。」（不误报立即生效/需重启）。
    #[test]
    fn update_config_no_change_reports_no_change() {
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_none_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            c.host = "h1".to_string();
        });

        let resp = svc
            .update_config(UpdateConfigRequest {
                host: Some("h1".to_string()),
                ..Default::default()
            })
            .expect("同值提交应成功");
        assert_eq!(resp.message, "无改动。", "同值提交必须回「无改动。」");
        assert!(!resp.restart_required);
        assert_eq!(disk_config_json(&path)["host"], "h1", "同值提交磁盘不变");
    }

    /// 🔴 承重：error_messages 是 **per-key merge**——提交只更新提交的 key，
    /// 未提交的 key 保持磁盘原值；整表被校验拒绝时旧表保持（先校验再写盘）。
    ///
    /// 回退即 FAIL：把 merge 改成整表替换（`config.error_messages = em`），
    /// 未提交的 k2 会消失，断言失败。
    #[test]
    fn update_config_error_messages_merge_keeps_unsubmitted_keys() {
        use crate::model::error_messages::ErrorMessageOverride;
        let dir = TempDir(std::env::temp_dir().join(format!(
            "ks_upd_errmsg_{}",
            uuid::Uuid::new_v4()
        )));
        let (svc, path) = svc_with_disk_config(&dir, |c| {
            let mut table = HashMap::new();
            table.insert(
                "k1".to_string(),
                ErrorMessageOverride {
                    status: Some(429),
                    r#type: Some("rate_limit_error".to_string()),
                    message: Some("旧文案".to_string()),
                    retry_after_secs: Some(8),
                },
            );
            table.insert(
                "k2".to_string(),
                ErrorMessageOverride {
                    status: Some(500),
                    r#type: Some("api_error".to_string()),
                    message: Some("k2 保持".to_string()),
                    retry_after_secs: None,
                },
            );
            c.error_messages = table;
        });

        let mut submitted = HashMap::new();
        submitted.insert(
            "k1".to_string(),
            ErrorMessageOverride {
                status: Some(429),
                r#type: Some("rate_limit_error".to_string()),
                message: Some("新文案".to_string()),
                retry_after_secs: Some(8),
            },
        );
        svc.update_config(UpdateConfigRequest {
            error_messages: Some(submitted),
            ..Default::default()
        })
        .expect("合法 per-key 更新应成功");

        let disk = disk_config_json(&path);
        assert_eq!(
            disk["errorMessages"]["k1"]["message"], "新文案",
            "提交的 key 必须更新"
        );
        assert_eq!(
            disk["errorMessages"]["k2"]["message"], "k2 保持",
            "未提交的 key 必须保持（per-key merge，不是整表替换）"
        );

        // 整表被校验拒绝时旧表保持：提交一个非法 key → Err 且 k1 不被写坏。
        let mut bad = HashMap::new();
        bad.insert(
            "k1".to_string(),
            ErrorMessageOverride {
                status: Some(418),
                r#type: Some("api_error".to_string()),
                message: None,
                retry_after_secs: None,
            },
        );
        svc.update_config(UpdateConfigRequest {
            error_messages: Some(bad),
            ..Default::default()
        })
        .expect_err("非法错误码表必须整表拒绝");
        assert_eq!(
            disk_config_json(&path)["errorMessages"]["k1"]["message"], "新文案",
            "拒绝时必须保持旧表（先校验再写盘）"
        );
    }
}

#[cfg(test)]
mod update_refresh_token_tests {
    //! `update_refresh_token` 校验矩阵：截断拒 / 跨凭据重复拒 / 正常过（含自身原值重提交）。
    use super::super::*;

    fn mk_oauth_cred(id: u64, rt: &str) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("oauth".to_string());
        c.refresh_token = Some(rt.to_string());
        c
    }

    fn mk_service(rt1: &str, rt2: &str) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![mk_oauth_cred(1, rt1), mk_oauth_cred(2, rt2)],
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    fn token_hash(svc: &AdminService, id: u64) -> Option<String> {
        svc.token_manager
            .snapshot()
            .entries
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.refresh_token_hash.clone())
    }

    /// 截断 token（长度 <100 / 含 "..."）必须被拒：静默接受会让下一次刷新必然失败。
    #[test]
    fn truncated_token_is_rejected_with_400() {
        let svc = mk_service(&"a".repeat(150), &"b".repeat(150));
        for bad in ["a".repeat(99), "a".repeat(150) + "..."] {
            let err = svc
                .update_refresh_token(1, bad)
                .expect_err("截断 token 必须被拒");
            assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
            assert_eq!(
                err.status_code(),
                axum::http::StatusCode::BAD_REQUEST,
                "校验失败必须返回 400"
            );
            assert!(err.to_string().contains("截断"), "文案应说明截断，实际 {err}");
        }
    }

    /// 与其他凭据的 refresh_token 重复必须被拒（对齐 add_credential 的哈希去重）。
    /// 跨凭据重复用 `DuplicateCredential`（非 `InvalidCredential`）：#13 语言耦合改造后
    /// 该变体是前端「duplicate_credential」判别的唯一依据，不能随文案改写而失配。
    #[test]
    fn duplicate_token_across_credentials_is_rejected() {
        let rt1 = "a".repeat(150);
        let rt2 = "b".repeat(150);
        let svc = mk_service(&rt1, &rt2);
        let err = svc
            .update_refresh_token(2, rt1.clone())
            .expect_err("与凭据 1 相同的 token 必须被拒");
        assert!(matches!(
            err,
            AdminServiceError::DuplicateCredential(_)
        ));
        assert_eq!(
            err.status_code(),
            axum::http::StatusCode::BAD_REQUEST,
            "校验失败必须返回 400"
        );
        assert!(err.to_string().contains("重复"), "文案应说明重复，实际 {err}");
        // 被拒后 2 号原值不得被改动。
        assert_eq!(
            token_hash(&svc, 2).as_deref(),
            Some(sha256_hex(&rt2).as_str())
        );
    }

    /// 正常 token 通过；用自身当前值重提交（no-op）也必须通过 —— 去重必须排除自己。
    #[test]
    fn valid_token_passes_and_self_resubmit_is_allowed() {
        let rt1 = "a".repeat(150);
        let rt2 = "b".repeat(150);
        let svc = mk_service(&rt1, &rt2);
        let new_rt = "c".repeat(150);
        svc.update_refresh_token(1, new_rt.clone())
            .expect("正常 token 必须通过");
        assert_eq!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&new_rt).as_str())
        );
        // 自身当前值重提交：不得被跨凭据重复检测误伤。
        svc.update_refresh_token(1, new_rt.clone())
            .expect("用自身当前值重提交必须通过（去重排除自身）");
        assert_eq!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&new_rt).as_str())
        );
    }

    fn mk_api_key_cred(id: u64) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test".to_string());
        c
    }

    /// 🔴 对抗审查 MINOR-6（2026-08-15）：api_key 凭据没有 refreshToken 概念
    /// （直接用 kiro_api_key 作 Bearer），更新它是误操作，必须 400 且不动原值。
    #[test]
    fn api_key_credential_update_refresh_token_is_rejected() {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![mk_api_key_cred(1)],
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        let svc = AdminService::new(tm, Vec::<String>::new());
        let err = svc
            .update_refresh_token(1, "a".repeat(150))
            .expect_err("api_key 凭据更新 refreshToken 必须被拒");
        assert!(matches!(err, AdminServiceError::InvalidCredential(_)));
        assert_eq!(
            err.status_code(),
            axum::http::StatusCode::BAD_REQUEST,
            "凭据类型闸必须返回 400"
        );
        assert!(
            err.to_string().contains("OAuth"),
            "文案应说明仅 OAuth 凭据支持，实际 {err}"
        );
        // 被拒后原凭据不得被改动（refresh_token 仍为 None，无新哈希）。
        assert_eq!(token_hash(&svc, 1), None, "api_key 号不得被写入 refresh_token_hash");
    }

    /// 🔴 对抗审查 MINOR-7（2026-08-15）：从聊天工具粘贴的 token 常带首尾换行/空白/
    /// 引号，entry 处 trim 后通过校验，落库（refresh_token_hash）必须是 trim 后的
    /// 规范值 —— 脏空白不得进入哈希，否则刷新链路对不上。
    #[test]
    fn whitespace_wrapped_token_is_trimmed_before_validate_and_store() {
        let rt1 = "a".repeat(150);
        let rt2 = "b".repeat(150);
        let svc = mk_service(&rt1, &rt2);
        let new_rt = "c".repeat(150);
        let wrapped = format!("\n\t\"{}\" \n", new_rt);
        let trimmed = wrapped.trim().to_string();
        svc.update_refresh_token(1, wrapped.clone())
            .expect("trim 后应通过校验（长度/截断检查作用于 trim 后值）");
        assert_eq!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&trimmed).as_str()),
            "落库哈希必须是 trim 后的值（首尾空白不得进入哈希）"
        );
        assert_ne!(
            token_hash(&svc, 1).as_deref(),
            Some(sha256_hex(&wrapped).as_str()),
            "未 trim 的原始串不得作为哈希（否则下次刷新 invalid_grant）"
        );
    }
}

#[cfg(test)]
mod batch_op_tests {
    //! P2-10：batch-reset / batch-disabled / batch-allowed-models / batch-refresh
    //! 部分失败仍 200，逐条 `results`（与 batch-delete 同款）。
    use super::super::*;

    fn mk_api_key(id: u64) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some(format!("ksk_batch_{id}"));
        c
    }

    fn mk_service(creds: Vec<KiroCredentials>) -> AdminService {
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造 token manager"),
        );
        AdminService::new(tm, Vec::<String>::new())
    }

    fn assert_mixed(results: &[BatchDeleteItemResult], ok_id: u64, fail_id: u64) {
        assert_eq!(results.len(), 2, "results 顺序与请求 ids 一致，不得丢条");
        assert_eq!(results[0].id, ok_id);
        assert!(results[0].ok, "存在的凭据必须成功，error={:?}", results[0].error);
        assert!(results[0].error.is_none());
        assert_eq!(results[1].id, fail_id);
        assert!(!results[1].ok, "缺失 id 必须失败且不拖垮成功条");
        assert!(
            results[1].error.as_ref().is_some_and(|e| e.contains("不存在")),
            "失败条必须带原因，实际 {:?}",
            results[1].error
        );
    }

    #[test]
    fn batch_reset_mixed_success_and_missing_id() {
        let svc = mk_service(vec![mk_api_key(1)]);
        let results = svc.reset_credentials_batch(&[1, 999]);
        assert_mixed(&results, 1, 999);
        let snap = svc.get_all_credentials();
        assert_eq!(snap.credentials.len(), 1);
        assert!(!snap.credentials[0].disabled);
        assert_eq!(snap.credentials[0].failure_count, 0);
    }

    #[test]
    fn batch_disable_mixed_success_and_missing_id() {
        let svc = mk_service(vec![mk_api_key(1)]);
        let results = svc.set_disabled_batch(&[1, 999], true);
        assert_mixed(&results, 1, 999);
        let snap = svc.get_all_credentials();
        assert!(snap.credentials[0].disabled, "成功条必须已禁用");
    }

    #[test]
    fn batch_allowed_models_mixed_success_and_missing_id() {
        let svc = mk_service(vec![mk_api_key(1)]);
        let models = Some(vec!["deepseek-3.2".to_string()]);
        let results = svc.set_allowed_models_batch(&[1, 999], models.clone());
        assert_mixed(&results, 1, 999);
        let snap = svc.get_all_credentials();
        assert_eq!(
            snap.credentials[0].allowed_models,
            Some(vec!["deepseek-3.2".to_string()])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_refresh_api_key_and_missing_id_each_fail_in_results() {
        // api_key 无 refresh：本地 RefreshNotSupported，不算成功。
        // 两条都失败仍必须逐条落 results 且 HTTP 层会是 200（handler 不短路）。
        let svc = mk_service(vec![mk_api_key(1)]);
        let results = svc.force_refresh_tokens_batch(&[1, 999]).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 1);
        assert!(!results[0].ok, "api_key 强刷必须失败（无 refresh_token）");
        assert!(results[0].error.is_some());
        assert_eq!(results[1].id, 999);
        assert!(!results[1].ok);
        assert!(
            results[1].error.as_ref().is_some_and(|e| e.contains("不存在")),
            "缺失 id 必须失败，实际 {:?}",
            results[1].error
        );
    }

    /// 再加一条真正的成功/失败混合：oauth 号 reset 成功 + 缺失 id 失败。
    /// refresh 的成功路径要打上游，不在本仓单测里跑。
    #[test]
    fn batch_results_keep_request_order_after_partial_failure() {
        let svc = mk_service(vec![mk_api_key(1), mk_api_key(2)]);
        let results = svc.reset_credentials_batch(&[2, 999, 1]);
        assert_eq!(results.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2, 999, 1]);
        assert!(results[0].ok);
        assert!(!results[1].ok);
        assert!(results[2].ok);
    }

    #[test]
    fn batch_endpoints_are_wired_in_router() {
        let router = include_str!("router.rs");
        let compact: String = router.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接：路径与 handler 名拆开，避免将来若守卫改读自身源码时自匹配。
        for (path, handler) in [
            ("batch-reset", "reset_credentials_batch"),
            ("batch-disabled", "set_credentials_disabled_batch"),
            ("batch-allowed-models", "set_credentials_allowed_models_batch"),
            ("batch-refresh", "force_refresh_tokens_batch"),
        ] {
            let route = format!("\"/credentials/{path}\",post({handler}");
            assert!(
                compact.contains(&route),
                "批量端点必须注册进鉴权路由树：{route}"
            );
        }
    }

    #[test]
    fn batch_request_bodies_parse_camel_case() {
        use crate::admin::types::{
            BatchIdsRequest, BatchSetAllowedModelsRequest, BatchSetDisabledRequest,
        };
        let ids: BatchIdsRequest =
            serde_json::from_str(r#"{"ids":[1,2]}"#).expect("ids 体应能解析");
        assert_eq!(ids.ids, vec![1, 2]);
        let dis: BatchSetDisabledRequest =
            serde_json::from_str(r#"{"ids":[3],"disabled":true}"#).expect("disabled 体应能解析");
        assert!(dis.disabled);
        assert_eq!(dis.ids, vec![3]);
        let wl: BatchSetAllowedModelsRequest =
            serde_json::from_str(r#"{"ids":[4],"allowedModels":["glm-5"]}"#)
                .expect("allowedModels camelCase 应能解析");
        assert_eq!(wl.allowed_models, Some(vec!["glm-5".to_string()]));
        let clear: BatchSetAllowedModelsRequest =
            serde_json::from_str(r#"{"ids":[4]}"#).expect("缺 allowedModels 应为 None");
        assert!(clear.allowed_models.is_none());
    }
}
